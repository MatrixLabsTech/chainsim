//! Solana acceptance tests. They start a real Surfpool fork of Solana Mainnet
//! in Docker against the public upstream, so they are ignored by default:
//!
//!     cargo test -p chainsim --test solana -- --ignored --test-threads=1 --nocapture

use std::process::Command;
use std::time::Instant;

use serde_json::{json, Value};

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// PayPal USD, a Token-2022 mint.
const PYUSD: &str = "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
/// A wallet that holds SOL on Solana Mainnet.
const MAINNET_WALLET: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";

struct Out {
    code: i32,
    json: Value,
}

fn chainsim(args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_chainsim")).args(args).output().expect("run chainsim");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(stdout.lines().count(), 1, "stdout of {args:?} is not one line: {stdout}");
    let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout of {args:?} is not JSON ({e}): {stdout}"));
    assert!(chainsim_core::envelope::is_valid(&json), "not an envelope: {json}");
    Out { code: out.status.code().unwrap_or(-1), json }
}

fn ok(args: &[&str]) -> Value {
    let out = chainsim(args);
    assert_eq!(out.code, 0, "{args:?}: {}", out.json);
    out.json["data"].clone()
}

/// The command exits with `code` and `error.code`.
fn fails(args: &[&str], code: i32, error: &str) {
    let out = chainsim(args);
    assert_eq!((out.code, out.json["error"]["code"].as_str()), (code, Some(error)), "{args:?}: {}", out.json);
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

/// A new address nobody holds a key for: 32 bytes that end in the time.
fn fresh_address(tag: u8) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let mut bytes = [tag; 32];
    bytes[..4].copy_from_slice(&[0xc5, 0x1a, 0x7e, 0x57]);
    bytes[16..].copy_from_slice(&nanos.to_be_bytes());
    chainsim_solana::units::base58_encode(&bytes)
}

/// The engine and the sidecar run hardened, as non-root users, with no upstream URL in their arguments.
fn check_hardening(name: &str, published: bool) {
    let engine = format!("chainsim-{name}");
    for c in [engine.clone(), format!("chainsim-{name}-control")] {
        assert_eq!(inspect(&c, "{{.HostConfig.ReadonlyRootfs}}"), "true", "{c}");
        assert_eq!(inspect(&c, "{{json .HostConfig.CapDrop}}"), "[\"ALL\"]", "{c}");
        assert_eq!(inspect(&c, "{{json .HostConfig.SecurityOpt}}"), "[\"no-new-privileges\"]", "{c}");
        assert!(!inspect(&c, "{{json .Args}}").contains("://"), "{c}: a URL in the arguments");
        let user = inspect(&c, "{{.Config.User}}");
        assert!(!user.is_empty() && user != "root" && user != "0" && !user.starts_with("0:"), "{c} runs as {user:?}");
    }
    // The process itself, not only the configuration.
    assert_eq!(docker(&["exec", &engine, "id", "-u"]), "10001");
    assert_eq!(inspect(&engine, "{{.HostConfig.Memory}}"), (1u64 << 30).to_string());
    let ports = inspect(&engine, "{{json .NetworkSettings.Ports}}");
    if published {
        assert!(ports.contains("\"HostIp\":\"127.0.0.1\""), "{ports}");
        assert!(!ports.contains("0.0.0.0") && !ports.contains("\"::\""), "{ports}");
    } else {
        assert!(!ports.contains("HostPort"), "ports {ports}");
    }
}

fn call(name: &str, method: &str, params: Value) -> Value {
    ok(&["call", name, method, &params.to_string()])
}

fn lamports(name: &str, address: &str) -> u64 {
    call(name, "getBalance", json!([address, { "commitment": "processed" }]))["value"].as_u64().expect("a balance")
}

/// The owner's token accounts for a mint: (owner program, base units).
fn token_accounts(name: &str, owner: &str, mint: &str) -> Vec<(String, String)> {
    let v = call(name, "getTokenAccountsByOwner", json!([owner, { "mint": mint }, { "encoding": "jsonParsed", "commitment": "processed" }]));
    v["value"]
        .as_array()
        .expect("token accounts")
        .iter()
        .map(|a| (a["account"]["owner"].as_str().unwrap().to_string(), a["account"]["data"]["parsed"]["info"]["tokenAmount"]["amount"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
#[ignore]
fn solana_mainnet() {
    let name = "acc-sol";
    chainsim(&["down", name]);

    // describe names the family, the pinned engine and what the adapter does.
    let d = ok(&["describe", "solana-mainnet"]);
    assert_eq!((d["family"].as_str(), d["engine"].as_str(), d["maturity"].as_str()), (Some("solana"), Some("surfpool@1.7.0"), Some("experimental")));
    for op in ["up", "down", "status", "fund", "set", "call", "selftest"] {
        assert!(d["operations"].as_array().unwrap().contains(&json!(op)), "describe lacks {op}");
    }

    // 1. There is no pinned fork point: --block exits 3 and starts nothing.
    fails(&["up", "solana-mainnet", "--name", name, "--block", "1"], 3, "unsupported");
    assert_gone(name);

    let t = Instant::now();
    let fork = ok(&["up", "solana-mainnet", "--name", name]);
    eprintln!("up took {} ms", t.elapsed().as_millis());
    assert_eq!((fork["chain"].as_str(), fork["chainId"].as_str(), fork["maturity"].as_str()), (Some("solana-mainnet"), Some("solana:mainnet"), Some("experimental")));
    assert_eq!((fork["rpcUrl"].as_str(), fork["wsUrl"].as_str()), (Some("http://acc-sol:8899"), Some("ws://acc-sol:8900")));
    assert!(fork["hostRpcUrl"].is_null());
    assert_eq!(fork["forkPoint"]["kind"], "slot");
    let slot = fork["forkPoint"]["number"].as_u64().expect("a fork point slot");
    assert!(slot > 400_000_000, "fork point {slot}");
    let limits = fork["limits"].as_array().expect("limits");
    assert!(limits.iter().any(|l| l.as_str().unwrap().contains("no pinned fork point")), "{limits:?}");
    check_hardening(name, false);
    // A second up of a running fork returns it as is.
    assert_eq!(ok(&["up", "solana-mainnet", "--name", name])["forkPoint"], fork["forkPoint"]);
    assert_eq!(call(name, "getGenesisHash", json!([])), MAINNET_GENESIS);

    // 2. fund: SOL, USDC (Token) and PYUSD (Token-2022) read back exactly.
    let w = fresh_address(1);
    let funded = ok(&["fund", name, &w, "--native", "5", "--token", "USDC:1000", "--token", &format!("{PYUSD}:250")]);
    assert_eq!(
        funded["balances"],
        json!([
            { "asset": "SOL", "address": null, "decimals": 9, "amount": "5", "raw": "5000000000" },
            { "asset": "USDC", "address": USDC, "decimals": 6, "amount": "1000", "raw": "1000000000" },
            { "asset": PYUSD, "address": PYUSD, "decimals": 6, "amount": "250", "raw": "250000000" },
        ])
    );
    // Read back through raw calls, apart from fund's own check.
    assert_eq!(lamports(name, &w), 5_000_000_000);
    assert_eq!(token_accounts(name, &w, USDC), [(TOKEN_PROGRAM.to_string(), "1000000000".to_string())]);
    // One PYUSD account, owned by the Token-2022 program, and no legacy Token account beside it.
    assert_eq!(token_accounts(name, &w, PYUSD), [(TOKEN_2022_PROGRAM.to_string(), "250000000".to_string())]);

    // A mint that does not exist, and an account that is not a mint, exit 3 and write nothing.
    let w2 = fresh_address(2);
    let no_mint = fresh_address(3);
    fails(&["fund", name, &w2, "--token", &format!("{no_mint}:1")], 3, "unsupported");
    fails(&["fund", name, &w2, "--token", &format!("{MAINNET_WALLET}:1")], 3, "unsupported");
    fails(&["set", name, "token", &w2, &no_mint, "1"], 3, "unsupported");
    assert!(token_accounts(name, &w2, &no_mint).is_empty());
    fails(&["fund", name, &w2, "--token", "NOPE:1"], 4, "unknown_token");
    fails(&["fund", name, &w2], 2, "usage");
    fails(&["fund", name, "0xdeadbeef", "--native", "1"], 2, "usage");
    fails(&["fund", name, &w2, "--native", "0.0000000001"], 2, "usage");
    // An address that holds SOL on mainnet is refused by fund.
    fails(&["fund", name, MAINNET_WALLET, "--native", "1"], 7, "mainnet_address");

    // set balance and set token write an exact amount, also a lower one, on any address.
    let set = ok(&["set", name, "balance", &w, "0.25"]);
    assert_eq!((set["amount"].as_str(), set["raw"].as_str(), set["symbol"].as_str()), (Some("0.25"), Some("250000000"), Some("SOL")));
    assert_eq!(lamports(name, &w), 250_000_000);
    let set = ok(&["set", name, "token", &w, "USDC", "10"]);
    assert_eq!((set["raw"].as_str(), set["tokenProgram"].as_str(), set["mint"].as_str()), (Some("10000000"), Some(TOKEN_PROGRAM), Some(USDC)));
    let set = ok(&["set", name, "token", &w, PYUSD, "1.5"]);
    assert_eq!((set["raw"].as_str(), set["tokenProgram"].as_str()), (Some("1500000"), Some(TOKEN_2022_PROGRAM)));
    assert!(set["note"].as_str().is_some_and(|n| n.contains("chainsim status")));
    assert_eq!(token_accounts(name, &w, USDC), [(TOKEN_PROGRAM.to_string(), "10000000".to_string())]);
    assert_eq!(token_accounts(name, &w, PYUSD), [(TOKEN_2022_PROGRAM.to_string(), "1500000".to_string())]);
    assert_eq!(ok(&["set", name, "balance", MAINNET_WALLET, "3"])["raw"], "3000000000");
    assert_eq!(lamports(name, MAINNET_WALLET), 3_000_000_000);
    fails(&["set", name, "storage", &w, "0", "1"], 3, "unsupported");
    fails(&["set", name, "token", &w, "USDC", "1", "--from", MAINNET_WALLET], 3, "unsupported");

    // call: one raw RPC call, cheatcodes included.
    call(name, "surfnet_setAccount", json!([w2, { "lamports": 7 }]));
    assert_eq!(lamports(name, &w2), 7);
    fails(&["call", name, "getBalance", "{}"], 2, "usage");
    fails(&["call", name, "noSuchMethod"], 1, "rpc_error");

    // 3. selftest passes, and status lists each change in order.
    let st = ok(&["selftest", name]);
    let parts: Vec<&str> = st["parts"].as_array().unwrap().iter().map(|p| p["part"].as_str().unwrap()).collect();
    assert!(parts.starts_with(&["genesis-hash", "slot-advances", "mainnet-state", "fund-native", "fund-token"]), "{parts:?}");
    assert!(st["ok"] == true && st["parts"].as_array().unwrap().iter().all(|p| p["ok"] == true), "{st}");

    let status = ok(&["status", name]);
    assert!(status["head"].as_u64().unwrap() > slot, "head {} after fork point {slot}", status["head"]);
    assert_eq!(status["forkPoint"], fork["forkPoint"]);
    let mods = status["modifications"].as_array().expect("modifications");
    let got: Vec<(&str, &str, &str)> = mods
        .iter()
        .map(|m| (m["kind"].as_str().unwrap(), m["address"].as_str().or(m["method"].as_str()).unwrap(), m["amount"].as_str().unwrap_or("")))
        .collect();
    let throwaway = "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ";
    assert!(
        got.starts_with(&[
            ("fund", w.as_str(), "5"),
            ("fund", w.as_str(), "1000"),
            ("fund", w.as_str(), "250"),
            ("set-balance", w.as_str(), "0.25"),
            ("set-token", w.as_str(), "10"),
            ("set-token", w.as_str(), "1.5"),
            ("set-balance", MAINNET_WALLET, "3"),
            ("call", "surfnet_setAccount", ""),
            ("selftest-fund", throwaway, "1"),
            ("selftest-fund", throwaway, "1"),
        ]),
        "{got:?}"
    );
    assert_eq!(mods[2]["tokenProgram"], TOKEN_2022_PROGRAM);
    assert_eq!(mods[7]["params"], json!([w2, { "lamports": 7 }]));
    assert!(mods.iter().all(|m| m["at"].as_u64().is_some()));

    let ls = ok(&["ls"]);
    let row = ls.as_array().unwrap().iter().find(|f| f["name"] == name).expect("the fork in ls");
    assert_eq!((row["running"].clone(), row["chain"].as_str()), (json!(true), Some("solana-mainnet")));

    // down leaves no container, volume or network, and a second down is exit 0.
    assert_eq!(ok(&["down", name])["removed"], true);
    assert_gone(name);
    assert_eq!(ok(&["down", name])["removed"], false);
    fails(&["status", name], 4, "fork_not_found");
}

/// `up --publish` binds the RPC port on 127.0.0.1 only. The host-side call
/// needs the test to share the Docker daemon's network namespace.
#[test]
#[ignore]
fn solana_publish_binds_loopback_only() {
    let name = "acc-sol-pub";
    chainsim(&["down", name]);
    let fork = ok(&["up", "solana-mainnet", "--name", name, "--publish"]);
    let url = fork["hostRpcUrl"].as_str().expect("hostRpcUrl").to_string();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    check_hardening(name, true);
    assert_eq!(ok(&["status", name])["hostRpcUrl"], url.as_str());
    let out = Command::new("curl")
        .args(["-s", "-m", "30", "-X", "POST", "-H", "content-type: application/json", "--data", r#"{"jsonrpc":"2.0","id":1,"method":"getGenesisHash","params":[]}"#, &url])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("no JSON from {url} ({e}): {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(v["result"], MAINNET_GENESIS);
    ok(&["down", name]);
    assert_gone(name);
}
