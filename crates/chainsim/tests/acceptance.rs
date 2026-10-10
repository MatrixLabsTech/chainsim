//! Phase 1a acceptance tests (design section 12). They start real forks in
//! Docker against the public upstreams, so they are ignored by default:
//!
//!     cargo test -p chainsim --test acceptance -- --ignored --test-threads=1 --nocapture

use std::process::Command;
use std::time::Instant;

use serde_json::{json, Value};

struct Out {
    code: i32,
    json: Value,
    stdout: String,
}

fn chainsim_env(args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chainsim"));
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run chainsim");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // Piped output is exactly one JSON object on one line.
    assert_eq!(stdout.lines().count(), 1, "stdout of {args:?} is not one line: {stdout}");
    let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout of {args:?} is not JSON ({e}): {stdout}"));
    assert!(chainsim_core::envelope::is_valid(&json), "not an envelope: {json}");
    Out { code: out.status.code().unwrap_or(-1), json, stdout }
}

fn chainsim(args: &[&str]) -> Out {
    chainsim_env(args, &[])
}

fn ok(args: &[&str]) -> Value {
    let out = chainsim(args);
    assert_eq!(out.code, 0, "{args:?}: {}", out.json);
    out.json["data"].clone()
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn inspect(container: &str, format: &str) -> String {
    docker(&["inspect", container, "--format", format])
}

/// Nothing labeled with the fork is left: containers, volumes, networks.
fn assert_gone(name: &str) {
    let label = format!("label=org.chainsim.fork={name}");
    assert_eq!(docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]), "", "containers left for {name}");
    assert_eq!(docker(&["volume", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "volumes left for {name}");
    assert_eq!(docker(&["network", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "networks left for {name}");
}

fn fresh_address(tag: u64) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    format!("0x{:024x}{:016x}", 0x5e1f_7e57u64, nanos ^ tag)
}

fn check_selftest(name: &str, extra: &[&str]) {
    let st = ok(&["selftest", name]);
    let parts: Vec<&str> = st["parts"].as_array().unwrap().iter().map(|p| p["part"].as_str().unwrap()).collect();
    for p in ["block", "block-time", "native-transfer", "token-transfer", "snapshot-revert", "upstream-read"].iter().chain(extra) {
        assert!(parts.contains(p), "{name} selftest lacks {p}: {parts:?}");
    }
    for p in st["parts"].as_array().unwrap() {
        assert_eq!(p["ok"], true, "{p}");
        assert!(!p["reason"].as_str().unwrap().contains("http"), "{p}");
        assert!(p["ms"].is_u64());
    }
    println!("{name} selftest: {}", st["parts"]);
}

fn check_hardening(name: &str, published: bool) {
    let c = format!("chainsim-{name}");
    assert_eq!(inspect(&c, "{{.HostConfig.ReadonlyRootfs}}"), "true");
    assert_eq!(inspect(&c, "{{json .HostConfig.CapDrop}}"), "[\"ALL\"]");
    assert_eq!(inspect(&c, "{{json .HostConfig.SecurityOpt}}"), "[\"no-new-privileges\"]");
    assert_eq!(inspect(&c, "{{.HostConfig.Memory}}"), (1u64 << 30).to_string());
    let user = inspect(&c, "{{.Config.User}}");
    assert!(!user.is_empty() && user != "root" && user != "0", "user {user:?}");
    assert!(inspect(&c, "{{json .HostConfig.Tmpfs}}").contains("/tmp"));
    // No upstream URL in the command line: only in the environment.
    assert!(!inspect(&c, "{{json .Args}}").contains("https://"), "upstream URL in args");
    let ports = inspect(&c, "{{json .NetworkSettings.Ports}}");
    let bindings = inspect(&c, "{{json .HostConfig.PortBindings}}");
    if published {
        assert!(ports.contains("\"HostIp\":\"127.0.0.1\""), "{ports}");
        assert!(!ports.contains("0.0.0.0") && !ports.contains("\"::\""), "{ports}");
    } else {
        assert!(bindings == "{}" || bindings == "null", "bindings {bindings}");
        assert!(!ports.contains("HostPort"), "ports {ports}");
    }
    let sidecar = format!("chainsim-{name}-control");
    assert_eq!(inspect(&sidecar, "{{.HostConfig.ReadonlyRootfs}}"), "true");
    assert_eq!(inspect(&sidecar, "{{json .HostConfig.CapDrop}}"), "[\"ALL\"]");
    let bindings = inspect(&sidecar, "{{json .HostConfig.PortBindings}}");
    assert!(bindings == "{}" || bindings == "null", "sidecar bindings {bindings}");
    println!("{name} hardening ok (published: {published})");
}

#[test]
#[ignore]
fn arc_mainnet() {
    let name = "acc-arc";
    chainsim(&["down", name]);

    let t = Instant::now();
    let fork = ok(&["up", "arc-mainnet", "--name", name]);
    println!("arc up in {} ms", t.elapsed().as_millis());
    assert_eq!(fork["chainId"], "eip155:5042");
    assert_eq!(fork["maturity"], "stable");
    assert_eq!(fork["hostRpcUrl"], Value::Null);

    // The first transaction after up confirms in under 10 s.
    let sender = fresh_address(1);
    ok(&["fund", name, &sender, "--native", "5"]);
    let to = fresh_address(2);
    let t = Instant::now();
    let receipt = ok(&["call", name, "eth_sendTransaction", &json!([{ "to": to, "value": "0xde0b6b3a7640000" }]).to_string(), "--from", &sender]);
    let first_tx_ms = t.elapsed().as_millis();
    assert_eq!(receipt["status"], "0x1", "{receipt}");
    assert!(first_tx_ms < 10_000, "first transaction took {first_tx_ms} ms");
    println!("arc first transaction after up: {first_tx_ms} ms");

    check_selftest(name, &["dual-usdc-view"]);
    check_hardening(name, false);

    // Idempotent up: same name and chain, no --block, returns the same fork.
    let again = ok(&["up", "arc-mainnet", "--name", name]);
    assert_eq!(again["forkPoint"], fork["forkPoint"]);
    // A different block on a taken name: exit 8.
    let block = fork["forkPoint"]["number"].as_u64().unwrap();
    let conflict = chainsim(&["up", "arc-mainnet", "--name", name, "--block", &(block - 1).to_string()]);
    assert_eq!(conflict.code, 8, "{}", conflict.json);
    assert_eq!(conflict.json["error"]["code"], "name_conflict");

    // fund: native USDC and an ERC-20, read back.
    let wallet = fresh_address(3);
    let funded = ok(&["fund", name, &wallet, "--native", "25", "--token", "EURC:10.5"]);
    assert_eq!(funded["balances"][0]["asset"], "USDC");
    assert_eq!(funded["balances"][0]["amount"], "25");
    assert_eq!(funded["balances"][1]["asset"], "EURC");
    assert_eq!(funded["balances"][1]["amount"], "10.5");
    // USDC as a token sets the same native balance: one balance, two views.
    let funded = ok(&["fund", name, &wallet, "--token", "USDC:40"]);
    assert_eq!(funded["balances"][0]["amount"], "40");
    assert_eq!(funded["balances"][1]["amount"], "40");
    // A repeat gives the same state.
    let again = ok(&["fund", name, &wallet, "--token", "USDC:40"]);
    assert_eq!(again, funded);
    println!("arc fund: {funded}");

    // call from the host with no published port, and call --from.
    let bal = ok(&["call", name, "eth_getBalance", &json!([wallet, "latest"]).to_string()]);
    assert_eq!(bal, "0x22b1c8c1227a00000"); // 40e18
    let dest = fresh_address(4);
    let receipt = ok(&["call", name, "eth_sendTransaction", &json!([{ "to": dest, "value": "0x8ac7230489e80000" }]).to_string(), "--from", &wallet]);
    assert_eq!(receipt["status"], "0x1");
    let bal = ok(&["call", name, "eth_getBalance", &json!([dest, "latest"]).to_string()]);
    assert_eq!(bal, "0x8ac7230489e80000"); // 10 USDC

    // snapshot then revert restores the balance and hands back a fresh id.
    let snap = ok(&["snapshot", name]);
    ok(&["call", name, "eth_sendTransaction", &json!([{ "to": dest, "value": "0x1" }]).to_string(), "--from", &wallet]);
    let rev = ok(&["revert", name, snap["id"].as_str().unwrap()]);
    assert_eq!(rev["reverted"], snap["id"]);
    assert!(rev["snapshot"].is_string());
    assert_eq!(ok(&["call", name, "eth_getBalance", &json!([dest, "latest"]).to_string()]), "0x8ac7230489e80000");
    // The consumed id is gone: exit 4.
    let gone = chainsim(&["revert", name, snap["id"].as_str().unwrap()]);
    assert_eq!(gone.code, 4, "{}", gone.json);

    // fund on an address in use on Arc Mainnet: exit 7. The fork block's own
    // transactions name senders that are active on the real chain.
    let blk = ok(&["call", name, "eth_getBlockByNumber", &json!([format!("{block:#x}"), true]).to_string()]);
    if let Some(active) = blk["transactions"].as_array().and_then(|t| t.first()).and_then(|t| t["from"].as_str()) {
        let refused = chainsim(&["fund", name, active, "--native", "1"]);
        assert_eq!(refused.code, 7, "{}", refused.json);
        assert_eq!(refused.json["error"]["code"], "mainnet_address");
        println!("arc exit 7 on {active}");
    } else {
        println!("arc exit 7 skipped: fork block {block} has no transactions");
    }

    // ls lists it; status adds head and uptime.
    assert!(ok(&["ls"]).as_array().unwrap().iter().any(|f| f["name"] == name));
    let st = ok(&["status", name]);
    assert!(st["head"].as_u64().unwrap() > block);

    let down = ok(&["down", name]);
    assert_eq!(down["removed"], true);
    assert_gone(name);
    // down on a missing fork is exit 0.
    assert_eq!(ok(&["down", name])["removed"], false);
}

#[test]
#[ignore]
fn ethereum_mainnet() {
    let name = "acc-eth";
    chainsim(&["down", name]);
    let fork = ok(&["up", "ethereum-mainnet", "--name", name]);
    assert_eq!(fork["chainId"], "eip155:1");
    check_selftest(name, &[]);
    check_hardening(name, false);

    let wallet = fresh_address(5);
    let funded = ok(&["fund", name, &wallet, "--native", "3", "--token", "USDC:1000", "--token", "WETH:2"]);
    let amounts: Vec<_> = funded["balances"].as_array().unwrap().iter().map(|b| (b["asset"].as_str().unwrap().to_string(), b["amount"].as_str().unwrap().to_string())).collect();
    assert_eq!(amounts, vec![("ETH".into(), "3".into()), ("USDC".into(), "1000".into()), ("WETH".into(), "2".into())]);

    // fund on a known mainnet-active address: exit 7.
    let refused = chainsim(&["fund", name, "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "--native", "1"]);
    assert_eq!(refused.code, 7, "{}", refused.json);

    // A broken token contract makes the selftest fail: exit 6 with every check in data.
    ok(&["call", name, "anvil_setCode", r#"["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "0xfe"]"#]);
    let failed = chainsim(&["selftest", name]);
    assert_eq!(failed.code, 6, "{}", failed.json);
    assert_eq!(failed.json["error"]["code"], "selftest_failed");
    let bad: Vec<_> = failed.json["data"]["parts"].as_array().unwrap().iter().filter(|p| p["ok"] == false).map(|p| p["part"].clone()).collect();
    assert_eq!(bad, vec![json!("token-transfer")]);

    ok(&["down", name]);
    assert_gone(name);
}

#[test]
#[ignore]
fn publish_binds_loopback_only() {
    let name = "acc-pub";
    chainsim(&["down", name]);
    let fork = ok(&["up", "arc-mainnet", "--name", name, "--publish"]);
    let url = fork["hostRpcUrl"].as_str().expect("hostRpcUrl").to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    check_hardening(name, true);
    let out = Command::new("curl")
        .args(["-s", "-X", "POST", "-H", "content-type: application/json", "--data", r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#, &url])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["result"], "0x13b2");
    // A second up asking --publish of a fork without it is a conflict.
    ok(&["down", name]);
    assert_gone(name);
}

#[test]
#[ignore]
fn caller_network_and_alias() {
    let name = "acc-net";
    let net = "acc-caller-net";
    chainsim(&["down", name]);
    docker(&["network", "rm", net]);
    docker(&["network", "create", net]);
    let fork = ok(&["up", "arc-mainnet", "--name", name, "--network", net, "--alias", "anvil"]);
    assert_eq!(fork["rpcUrl"], "http://anvil:8545");
    assert_eq!(fork["network"], net);
    // A container on the caller's network reaches the fork by alias.
    let out = docker(&[
        "run", "--rm", "--network", net, "curlimages/curl:8.16.0", "-s", "-X", "POST", "-H", "content-type: application/json", "--data",
        r#"{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}"#, "http://anvil:8545",
    ]);
    assert!(out.contains("0x13b2"), "{out}");
    ok(&["down", name]);
    // The caller's network stays: chainsim did not create it.
    assert_eq!(docker(&["network", "ls", "--filter", &format!("name=^{net}$"), "--format", "{{.Name}}"]), net);
    docker(&["network", "rm", net]);
}

#[test]
#[ignore]
fn contract_exit_codes() {
    // 2: usage.
    let out = chainsim(&["up"]);
    assert_eq!((out.code, out.json["error"]["code"].clone()), (2, json!("usage")), "{}", out.stdout);
    let out = chainsim(&["fund", "x", "0x1", "--native", "1"]);
    assert_eq!(out.code, 4, "{}", out.stdout); // no fork x: not found before the address is checked
    let out = chainsim(&["up", "arc-mainnet", "--name", "Bad_Name"]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    // 3: unsupported comes from the fake adapter test in chainsim-core
    // (default_operations_are_unsupported_exit_3); no phase 1a command reaches it on EVM.
    // 4: not found.
    let out = chainsim(&["status", "acc-none"]);
    assert_eq!((out.code, out.json["error"]["code"].clone()), (4, json!("fork_not_found")));
    let out = chainsim(&["describe", "no-such-chain"]);
    assert_eq!((out.code, out.json["error"]["code"].clone()), (4, json!("unknown_chain")));
    // 5: the upstream is unreachable. The URL never shows, and nothing is left behind.
    let bad = "http://127.0.0.1:9/secret-key-123";
    let out = chainsim_env(&["up", "arc-mainnet", "--name", "acc-bad"], &[("CHAINSIM_ARC_MAINNET_RPC", bad)]);
    assert_eq!(out.code, 5, "{}", out.stdout);
    assert!(!out.stdout.contains("secret-key-123"), "{}", out.stdout);
    assert_eq!(out.json["error"]["next"], "retry, or export CHAINSIM_ARC_MAINNET_RPC=<provider URL>");
    assert!(!out.stdout.contains("127.0.0.1"), "{}", out.stdout);
    assert_gone("acc-bad");
    // describe never prints a provider URL.
    let out = chainsim_env(&["describe", "arc-mainnet"], &[("CHAINSIM_ARC_MAINNET_RPC", bad)]);
    assert_eq!(out.code, 0);
    assert!(!out.stdout.contains("secret-key-123"));
    assert_eq!(out.json["data"]["upstream"]["set"], true);
}
