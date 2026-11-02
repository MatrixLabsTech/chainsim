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

/// A contract that reads a HyperCore oracle price the way Solidity callers
/// do: it forwards its calldata (`abi.encode(uint32 perp)`) to 0x807 with
/// STATICCALL, bubbles a revert up, and returns the 32-byte answer.
const PRICE_READER: &str = concat!(
    "0x",
    "365f5f37",     // calldatacopy(0, 0, calldatasize)
    "5f5f365f",     // retSize 0, retOffset 0, argsSize calldatasize, argsOffset 0
    "6108075afa",   // staticcall(gas, 0x807, ...)
    "3d5f5f3e",     // returndatacopy(0, 0, returndatasize)
    "61001857",     // success: jump to 0x18
    "3d5ffd",       // revert(0, returndatasize)
    "5b3d602014",   // 0x18: returndatasize == 32
    "61002457",     // yes: jump to 0x24
    "5f5ffd",       // revert(0, 0)
    "5b60205ff3",   // 0x24: return(0, 32)
);

/// The CoreDepositWallet, which holds USDC on HyperEVM Mainnet.
const HYPEREVM_USDC_HOLDER: &str = "0x6B9E773128f453f5c2C60935Ee2DE2CBc5390A24";
const HYPEREVM_USDC: &str = "0xb88339CB7199b77E23DB6E890353E22632Ba630f";

fn word(n: u64) -> String {
    format!("0x{n:064x}")
}

/// The oracle price the reader returns for a perp, or the failed call.
fn read_price(name: &str, reader: &str, perp: u64) -> Result<u64, Out> {
    let params = json!([{ "to": reader, "data": word(perp) }, "latest"]).to_string();
    let out = chainsim(&["call", name, "eth_call", &params]);
    if out.code != 0 {
        return Err(out);
    }
    Ok(u64::from_str_radix(out.json["data"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap())
}

#[test]
#[ignore]
fn hyperevm_mainnet() {
    let name = "acc-hyperevm";
    chainsim(&["down", name]);

    // --block on the default upstream: refused, and nothing is created.
    let refused = chainsim_env(&["up", "hyperevm-mainnet", "--name", name, "--block", "1000000"], &[("CHAINSIM_HYPEREVM_MAINNET_RPC", "")]);
    assert_eq!((refused.code, refused.json["error"]["code"].as_str()), (7, Some("latest_state_only")), "{}", refused.json);
    assert!(refused.json["error"]["next"].as_str().unwrap().contains("CHAINSIM_HYPEREVM_MAINNET_RPC"));
    assert_gone(name);

    let t = Instant::now();
    let fork = ok(&["up", "hyperevm-mainnet", "--name", name]);
    println!("up: {:.1} s", t.elapsed().as_secs_f64());
    assert_eq!(fork["chainId"], "eip155:999");
    let hc = &fork["hypercore"];
    assert_eq!(hc["seeded"], 17, "{hc}");
    assert!(hc["l1BlockNumber"].as_u64().unwrap() > 0);
    let coins: Vec<_> = hc["perps"].as_array().unwrap().iter().map(|p| p["coin"].as_str().unwrap().to_string()).collect();
    assert_eq!(coins, ["BTC", "ETH", "SOL", "HYPE"]);
    let btc_oracle: u64 = hc["perps"][0]["oraclePx"].as_str().unwrap().parse().unwrap();
    println!("seeded: {hc}");
    let t = Instant::now();
    assert_eq!(ok(&["up", "hyperevm-mainnet", "--name", name])["hypercore"], *hc, "up on a running fork returns it as is");
    println!("up of the running fork: {:.1} s", t.elapsed().as_secs_f64());
    check_hardening(name, false);
    let t = Instant::now();
    check_selftest(name, &["chain-id", "hypercore", "corewriter"]);
    println!("selftest: {:.1} s", t.elapsed().as_secs_f64());

    // A contract that calls 0x807 reads the seeded price, then the one set.
    let reader = "0x000000000000000000000000000000000000c0de";
    ok(&["set", name, "code", reader, PRICE_READER]);
    assert_eq!(read_price(name, reader, 0).ok(), Some(btc_oracle));
    let set = ok(&["set", name, "hypercore", "price", "0", "123456"]);
    assert_eq!(set["sets"].as_array().unwrap().len(), 2, "{set}");
    assert_eq!(read_price(name, reader, 0).ok(), Some(123456));
    let mark = ok(&["call", name, "eth_call", &json!([{ "to": "0x0000000000000000000000000000000000000806", "data": word(0) }, "latest"]).to_string()]);
    assert_eq!(mark, json!(word(123456)));

    // An unseeded query reverts with the chainsim reason, and --live copies the upstream's answer.
    let unseeded = read_price(name, reader, 2).unwrap_err();
    assert_eq!(unseeded.code, 1);
    let reason = chainsim_evm::hypercore::UNSEEDED_REASON;
    let reason_hex = alloy_hex(reason);
    assert!(unseeded.stdout.contains(reason) || unseeded.stdout.contains(&reason_hex), "{}", unseeded.stdout);
    let live = ok(&["set", name, "precompile", "0x807", &word(2), "--live"]);
    assert_eq!(live["source"], "live");
    let live_px = u64::from_str_radix(live["result"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    assert!(live_px > 0);
    assert_eq!(read_price(name, reader, 2).ok(), Some(live_px));

    // fund, then send as a USDC holder.
    let wallet = fresh_address(9);
    let t = Instant::now();
    let funded = ok(&["fund", name, &wallet, "--native", "10", "--token", "USDC:1000"]);
    println!("fund: {:.1} s", t.elapsed().as_secs_f64());
    let amounts: Vec<_> = funded["balances"].as_array().unwrap().iter().map(|b| (b["asset"].as_str().unwrap().to_string(), b["amount"].as_str().unwrap().to_string())).collect();
    assert_eq!(amounts, vec![("HYPE".into(), "10".into()), ("USDC".into(), "1000".into())]);
    let t = Instant::now();
    let sent = ok(&["send", name, "--as", HYPEREVM_USDC_HOLDER, "--to", HYPEREVM_USDC, "--sig", "transfer(address,uint256)", &wallet, "250000000"]);
    println!("send --as: {:.1} s", t.elapsed().as_secs_f64());
    assert_eq!(sent["status"], 1);
    let data = format!("0x70a08231{:0>64}", wallet.trim_start_matches("0x"));
    let bal = ok(&["call", name, "eth_call", &json!([{ "to": HYPEREVM_USDC, "data": data }, "latest"]).to_string()]);
    assert_eq!(u64::from_str_radix(bal.as_str().unwrap().trim_start_matches("0x"), 16).unwrap(), 1_250_000_000);

    // A limit order sent through CoreWriter shows decoded.
    let order = chainsim_evm::hypercore::limit_order(0, true, 900_000, 100, false, 2, 0);
    let order_hex = chainsim_evm::hypercore::to_hex(&order);
    let sent = ok(&["send", name, "--as", &wallet, "--to", "0x3333333333333333333333333333333333333333", "--sig", "sendRawAction(bytes)", &order_hex]);
    assert_eq!(sent["status"], 1);
    let actions = ok(&["call", name, "corewriter_actions"]);
    let last = actions.as_array().unwrap().last().cloned().unwrap_or_default();
    println!("corewriter_actions: {actions}");
    assert_eq!(last["action"], "limitOrder");
    assert_eq!(last["user"].as_str().unwrap().to_lowercase(), wallet.to_lowercase());
    assert_eq!(last["fields"], json!({ "asset": "0", "isBuy": true, "limitPx": "900000", "sz": "100", "reduceOnly": false, "encodedTif": "2", "cloid": "0" }));
    let status = ok(&["status", name]);
    assert_eq!(status["hypercore"]["coreWriterActions"], 1);
    let kinds: Vec<_> = status["modifications"].as_array().unwrap().iter().map(|m| m["kind"].as_str().unwrap().to_string()).collect();
    assert!(kinds.contains(&"set-precompile".to_string()) && kinds.contains(&"send-as".to_string()), "{kinds:?}");

    // Snapshot, a price set, revert: the old price is back.
    let snap = ok(&["snapshot", name]);
    ok(&["set", name, "hypercore", "price", "0", "999"]);
    assert_eq!(read_price(name, reader, 0).ok(), Some(999));
    let t = Instant::now();
    ok(&["revert", name, snap["id"].as_str().unwrap()]);
    println!("revert: {:.1} s", t.elapsed().as_secs_f64());
    assert_eq!(read_price(name, reader, 0).ok(), Some(123456));

    ok(&["down", name]);
    assert_gone(name);
}

/// A contract that forwards its calldata to the 0x800 position precompile
/// with STATICCALL and returns the answer, or bubbles the revert up.
const POSITION_READER: &str = concat!(
    "0x",
    "365f5f37",   // calldatacopy(0, 0, calldatasize)
    "5f5f365f",   // retSize 0, retOffset 0, argsSize calldatasize, argsOffset 0
    "6108005afa", // staticcall(gas, 0x800, ...)
    "3d5f5f3e",   // returndatacopy(0, 0, returndatasize)
    "61001857",   // success: jump to 0x18
    "3d5ffd",     // revert(0, returndatasize)
    "5b3d5ff3",   // 0x18: return(0, returndatasize)
);

/// HLP's strategy vaults, which hold perp positions on HyperCore mainnet.
const HLP_CHILDREN: &[&str] = &[
    "0x010461c14e146ac35fe42271bdc1134ee31c703a",
    "0x2e3d94f0562703b25c83308a05046ddaf9a8dd14",
    "0x2ed5c4484ea3ff8b57d5f2fb152a40d9f2b68308",
    "0x31ca8395cf837de08b24da3f660e77761dfb974b",
    "0x469f690213c467c39a23efacfd2816896009d7d8",
    "0x5e177e5e39c0f4e421f5865a6d8beed8d921cb70",
    "0xb0a55f13d22f66e6d495ac98113841b2326e9540",
];

/// The signed szi of a user's BTC (perp 0) position, read through the reader contract.
fn read_btc_szi(name: &str, reader: &str, user: &str) -> i64 {
    let data = format!("0x{:0>64}{:064x}", user.trim_start_matches("0x").to_lowercase(), 0);
    let out = ok(&["call", name, "eth_call", &json!([{ "to": reader, "data": data }, "latest"]).to_string()]);
    let hex = out.as_str().unwrap().trim_start_matches("0x").to_string();
    assert_eq!(hex.len(), 320, "Position is 5 words: {hex}");
    u64::from_str_radix(&hex[48..64], 16).unwrap() as i64
}

fn btc_position(state: &Value) -> Option<Value> {
    state["clearinghouseState"]["assetPositions"].as_array()?.iter().map(|p| p["position"].clone()).find(|p| p["coin"] == "BTC")
}

fn send_action(name: &str, user: &str, payload: &[u8]) -> Value {
    let hex = chainsim_evm::hypercore::to_hex(payload);
    let sent = ok(&["send", name, "--as", user, "--to", "0x3333333333333333333333333333333333333333", "--sig", "sendRawAction(bytes)", &hex]);
    assert_eq!(sent["status"], 1);
    let actions = ok(&["call", name, "corewriter_actions"]);
    actions.as_array().unwrap().last().cloned().unwrap()
}

fn native_balance(name: &str, who: &str) -> u128 {
    let out = ok(&["call", name, "eth_getBalance", &json!([who, "latest"]).to_string()]);
    u128::from_str_radix(out.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

/// The decimal string a fixed-point value of `d` decimals as an integer.
fn fixed(s: &str, d: u32) -> i128 {
    chainsim_evm::core::parse_dec(s, d).unwrap()
}

#[test]
#[ignore]
fn hyperevm_local_hypercore() {
    use chainsim_evm::hypercore::{encode_action, limit_order, word_addr, word_u};
    let name = "acc-hypercore";
    chainsim(&["down", name]);
    let started = Instant::now();

    let t = Instant::now();
    let fork = ok(&["up", "hyperevm-mainnet", "--name", name]);
    println!("up: {:.1} s", t.elapsed().as_secs_f64());
    let core = &fork["hypercore"]["core"];
    // metaAndAssetCtxs, spotMeta, and an l2Book and a fundingHistory per seeded perp.
    assert_eq!(core["infoCalls"], 10, "{core}");
    let btc = core["books"].as_array().unwrap().iter().find(|b| b["coin"] == "BTC").cloned().unwrap();
    let (best_bid, best_ask) = (fixed(btc["bestBid"].as_str().unwrap(), 8), fixed(btc["bestAsk"].as_str().unwrap(), 8));
    assert!(best_bid > 0 && best_ask > best_bid, "{btc}");

    // Mirror a mainnet account with a BTC position, and read it back through
    // the 0x800 precompile from a contract and through hypercore_state.
    let reader = "0x000000000000000000000000000000000000c0de";
    ok(&["set", name, "code", reader, POSITION_READER]);
    let mut mirrored = None;
    for user in HLP_CHILDREN {
        let t = Instant::now();
        let m = ok(&["set", name, "hypercore", "mirror", user]);
        println!("mirror {user}: {} positions, {:.1} s, {} info and {} upstream calls", m["positions"], t.elapsed().as_secs_f64(), m["infoCalls"], m["upstreamCalls"]);
        let state = ok(&["call", name, "hypercore_state", user]);
        if let Some(p) = btc_position(&state) {
            mirrored = Some((user.to_string(), p, state));
            break;
        }
    }
    let (whale, pos, state) = mirrored.expect("an HLP vault with a BTC position");
    assert!(fixed(state["clearinghouseState"]["marginSummary"]["accountValue"].as_str().unwrap(), 6) > 0);
    // BTC has szDecimals 5: the precompile's szi is the size times 1e5.
    let szi = fixed(pos["szi"].as_str().unwrap(), 5) as i64;
    assert_eq!(read_btc_szi(name, reader, &whale), szi, "{pos}");

    // Bridge HYPE in: Core spot rises by the amount.
    let wallet = fresh_address(21);
    ok(&["fund", name, &wallet, "--native", "10"]);
    let sent = ok(&["send", name, "--as", &wallet, "--to", "0x2222222222222222222222222222222222222222", "--value", "2"]);
    assert_eq!(sent["status"], 1);
    let state = ok(&["call", name, "hypercore_state", &wallet]);
    assert_eq!(state["spotClearinghouseState"]["balances"], json!([{ "coin": "HYPE", "token": 150, "total": "2.0", "hold": "0.0", "entryNtl": "0.0" }]));

    // USDC on spot, a USD class transfer to perp, then an IOC buy through
    // CoreWriter fills at the snapshot's best ask.
    ok(&["set", name, "hypercore", "spot-balance", &wallet, "0", "100000000000"]);
    let moved = send_action(name, &wallet, &encode_action(7, &[word_u(500_000_000), word_u(1)].concat()));
    assert_eq!(moved["result"], "moved 500.0 USD to perp", "{moved}");
    let t = Instant::now();
    let ioc = send_action(name, &wallet, &limit_order(0, true, (best_ask * 101 / 100) as u64, 100_000, false, 3, 0));
    println!("IOC buy: {:.1} s, {}", t.elapsed().as_secs_f64(), ioc["result"]);
    assert!(ioc["result"].as_str().unwrap().starts_with(&format!("filled 0.001 at {}, fee ", btc["bestAsk"].as_str().unwrap())), "{ioc}");
    assert_eq!(read_btc_szi(name, reader, &wallet), 100);
    let state = ok(&["call", name, "hypercore_state", &wallet]);
    assert_eq!(btc_position(&state).unwrap()["szi"], "0.001");

    // A GTC bid below the book rests, and a cancel by cloid removes it.
    let gtc = send_action(name, &wallet, &limit_order(0, true, (best_bid * 9 / 10) as u64, 100_000, false, 2, 77));
    assert!(gtc["result"].as_str().unwrap().starts_with("0.001 resting as oid"), "{gtc}");
    let state = ok(&["call", name, "hypercore_state", &wallet]);
    assert_eq!(state["openOrders"].as_array().unwrap().len(), 1, "{state}");
    let cancel = send_action(name, &wallet, &encode_action(11, &[word_u(0), word_u(77)].concat()));
    assert!(cancel["result"].as_str().unwrap().starts_with("canceled oid"), "{cancel}");
    assert_eq!(ok(&["call", name, "hypercore_state", &wallet])["openOrders"], json!([]));

    // Spot send to the HYPE system address credits the EVM balance.
    let before = native_balance(name, &wallet);
    let back = send_action(name, &wallet, &encode_action(6, &[word_addr("0x2222222222222222222222222222222222222222".parse().unwrap()), word_u(150), word_u(100_000_000)].concat()));
    assert!(back["result"].as_str().unwrap().contains("to the EVM"), "{back}");
    let after = native_balance(name, &wallet);
    assert!(after > before + 990_000_000_000_000_000, "1 HYPE back on the EVM, less gas: {before} -> {after}");
    let state = ok(&["call", name, "hypercore_state", &wallet]);
    let hype = state["spotClearinghouseState"]["balances"].as_array().unwrap().iter().find(|b| b["coin"] == "HYPE").cloned().unwrap();
    assert_eq!(hype["total"], "1.0", "{state}");

    // Snapshot, an order, revert: Core state is as before.
    let core_of = |s: Value| {
        let mut s = s;
        s["clearinghouseState"]["time"] = json!(0);
        s
    };
    let snap = ok(&["snapshot", name]);
    let held = core_of(ok(&["call", name, "hypercore_state", &wallet]));
    send_action(name, &wallet, &limit_order(0, true, (best_ask * 101 / 100) as u64, 200_000, false, 3, 0));
    assert_ne!(core_of(ok(&["call", name, "hypercore_state", &wallet])), held);
    ok(&["revert", name, snap["id"].as_str().unwrap()]);
    assert_eq!(core_of(ok(&["call", name, "hypercore_state", &wallet])), held);
    assert_eq!(read_btc_szi(name, reader, &wallet), 100);

    let status = ok(&["status", name]);
    println!("status hypercore: {}", status["hypercore"]["coreState"]);
    ok(&["down", name]);
    assert_gone(name);
    println!("whole run: {:.1} s", started.elapsed().as_secs_f64());
}

/// A wallet with 500 USD of perp margin on the copy's HyperCore.
fn margin_wallet(name: &str, tag: u64) -> String {
    use chainsim_evm::hypercore::{encode_action, word_u};
    let wallet = fresh_address(tag);
    ok(&["fund", name, &wallet, "--native", "1"]);
    ok(&["set", name, "hypercore", "spot-balance", &wallet, "0", "100000000000"]);
    let moved = send_action(name, &wallet, &encode_action(7, &[word_u(500_000_000), word_u(1)].concat()));
    assert_eq!(moved["result"], "moved 500.0 USD to perp", "{moved}");
    wallet
}

fn history(name: &str, user: &str) -> Vec<String> {
    let state = ok(&["call", name, "hypercore_state", user]);
    state["history"].as_array().unwrap().iter().map(|h| h["event"].as_str().unwrap().to_string()).collect()
}

#[test]
#[ignore]
fn hyperevm_market_scripting() {
    use chainsim_evm::hypercore::limit_order;
    let name = "acc-market";
    chainsim(&["down", name]);
    let started = Instant::now();

    let t = Instant::now();
    let fork = ok(&["up", "hyperevm-mainnet", "--name", name]);
    println!("up: {:.1} s, {} info calls", t.elapsed().as_secs_f64(), fork["hypercore"]["core"]["infoCalls"]);
    let core = &fork["hypercore"]["core"];
    println!("funding at up: {}", core["funding"]);
    let btc = core["books"].as_array().unwrap().iter().find(|b| b["coin"] == "BTC").cloned().unwrap();
    let (best_bid, best_ask) = (fixed(btc["bestBid"].as_str().unwrap(), 8), fixed(btc["bestAsk"].as_str().unwrap(), 8));
    let reader = "0x000000000000000000000000000000000000c0de";
    ok(&["set", name, "code", reader, POSITION_READER]);

    // A 0.1 BTC long at about 16x, and a resting bid 8% under the book.
    let long = margin_wallet(name, 31);
    let ioc = send_action(name, &long, &limit_order(0, true, (best_ask * 101 / 100) as u64, 10_000_000, false, 3, 0));
    let r = ioc["result"].as_str().unwrap();
    assert!(r.starts_with(&format!("filled 0.1 at {}, fee ", btc["bestAsk"].as_str().unwrap())), "{ioc}");
    assert_eq!(read_btc_szi(name, reader, &long), 10_000);
    let bidder = margin_wallet(name, 32);
    let bid_px = best_bid * 92 / 100 / 100_000_000 * 100_000_000;
    let gtc = send_action(name, &bidder, &limit_order(0, true, bid_px as u64, 1_000_000, false, 2, 0));
    assert!(gtc["result"].as_str().unwrap().contains("resting as oid"), "{gtc}");

    // A 10% drop: the bid fills as the price falls through it, and the long is liquidated.
    let drop_raw = (best_ask * 90 / 100 / 100_000_000) * 10;
    let t = Instant::now();
    let moved = ok(&["set", name, "hypercore", "price", "0", &drop_raw.to_string()]);
    println!("price drop: {:.1} s, {moved}", t.elapsed().as_secs_f64());
    assert_eq!(moved["filled"], 1, "{moved}");
    assert_eq!(moved["liquidated"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_lowercase()).collect::<Vec<_>>(), [long.to_lowercase()]);
    assert_eq!(read_btc_szi(name, reader, &long), 0);
    assert!(history(name, &long).last().unwrap().starts_with("liquidated: account value"), "{:?}", history(name, &long));
    assert_eq!(read_btc_szi(name, reader, &bidder), 1_000);
    assert!(history(name, &bidder).iter().any(|h| h.contains("as the price moved to")), "{:?}", history(name, &bidder));

    // Funding accrues once the clock passes an hour boundary.
    ok(&["set", name, "hypercore", "funding", "0", "0.0001"]);
    ok(&["time", name, "--advance", "3600"]);
    let state = ok(&["call", name, "hypercore_state", &bidder]);
    let cum = &btc_position(&state).unwrap()["cumFunding"]["sinceOpen"];
    assert!(fixed(cum.as_str().unwrap(), 6) > 0, "{state}");
    assert!(history(name, &bidder).iter().any(|h| h.starts_with("funding on BTC at rate 0.0001: paid")));

    // Replay a past day of BTC candles: the mark ends at the last close.
    let day = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / 86_400;
    let (from, to) = (((day - 2) * 86_400).to_string(), ((day - 1) * 86_400).to_string());
    let t = Instant::now();
    let replay = ok(&["time", name, "--replay", "BTC", "--from", &from, "--to", &to]);
    println!("replay: {:.1} s, {replay}", t.elapsed().as_secs_f64());
    assert!(replay["candles"].as_u64().unwrap() >= 24, "{replay}");
    assert_eq!(replay["infoCalls"], 1);
    let mark = ok(&["call", name, "eth_call", &json!([{ "to": "0x0000000000000000000000000000000000000806", "data": word(0) }, "latest"]).to_string()]);
    let mark = u64::from_str_radix(mark.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    assert_eq!(json!(mark), replay["markPx"]);
    assert_eq!(fixed(replay["lastClose"].as_str().unwrap(), 1) as u64, mark);

    // Snapshot, a price move, revert: positions and margin are as before.
    let core_of = |s: Value| {
        let mut s = s;
        s["clearinghouseState"]["time"] = json!(0);
        s
    };
    let snap = ok(&["snapshot", name]);
    let held = core_of(ok(&["call", name, "hypercore_state", &bidder]));
    ok(&["set", name, "hypercore", "price", "0", &(mark / 2).to_string()]);
    assert_ne!(core_of(ok(&["call", name, "hypercore_state", &bidder])), held);
    ok(&["revert", name, snap["id"].as_str().unwrap()]);
    assert_eq!(core_of(ok(&["call", name, "hypercore_state", &bidder])), held);

    ok(&["down", name]);
    assert_gone(name);
    println!("whole run: {:.1} s", started.elapsed().as_secs_f64());
}

fn alloy_hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}
