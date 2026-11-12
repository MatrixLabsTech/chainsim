//! Solana acceptance tests for prices: a Pyth price set at the fork's clock,
//! a real program that accepts it, and a replay of a past hour. They start a
//! real Surfpool fork of Solana Mainnet in Docker against the public upstream
//! and read the price source, so they are ignored by default:
//!
//!     cargo test -p chainsim --test solana_price -- --ignored --test-threads=1 --nocapture

use std::process::Command;

use chainsim_solana::price::{parse, Price};
use chainsim_solana::send::{base64_decode, base64_encode};
use chainsim_solana::units::base58_decode;
use serde_json::{json, Value};

/// The Pyth SOL/USD price account.
const SOL_USD: &str = "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE";
const BTC_USD: &str = "4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo";
const USDC_USD: &str = "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// A wallet that holds SOL on Solana Mainnet, the fee payer of the program calls.
const MAINNET_WALLET: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";
/// Save, the lending program, and the SOL reserve of its main pool. The
/// reserve names the Pyth SOL/USD account as its price oracle.
const SAVE: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
const SAVE_SOL_RESERVE: &str = "8PbodeaosQP19SjYFx855UMqWxH2HynZLdBXmsrbac36";
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";

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

/// The command exits with `code` and `error.code`. Returns the whole envelope.
fn fails(args: &[&str], code: i32, error: &str) -> Value {
    let out = chainsim(args);
    assert_eq!((out.code, out.json["error"]["code"].as_str()), (code, Some(error)), "{args:?}: {}", out.json);
    out.json
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn assert_gone(name: &str) {
    let label = format!("label=org.chainsim.fork={name}");
    assert_eq!(docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]), "", "containers left for {name}");
    assert_eq!(docker(&["volume", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "volumes left for {name}");
    assert_eq!(docker(&["network", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "networks left for {name}");
}

fn call(name: &str, method: &str, params: Value) -> Value {
    ok(&["call", name, method, &params.to_string()])
}

/// An account's bytes, read through a raw call.
fn account(name: &str, address: &str) -> Vec<u8> {
    let v = call(name, "getAccountInfo", json!([address, { "encoding": "base64", "commitment": "processed" }]));
    base64_decode(v["value"]["data"][0].as_str().expect("account data")).expect("base64")
}

/// The price account as the fork has it, read apart from chainsim's own read-back.
fn price(name: &str, address: &str) -> Price {
    parse(&account(name, address)).expect("a Pyth price account")
}

/// The Clock sysvar's Unix time.
fn clock(name: &str) -> i64 {
    let v = call(name, "getAccountInfo", json!([CLOCK, { "encoding": "jsonParsed", "commitment": "processed" }]));
    v["value"]["data"]["parsed"]["info"]["unixTimestamp"].as_i64().expect("a time")
}

/// Save's RefreshReserve for the SOL reserve, built here byte by byte: it
/// reads the reserve's Pyth and Switchboard oracles and refuses stale prices.
fn refresh_reserve(name: &str) -> String {
    let reserve = account(name, SAVE_SOL_RESERVE);
    // The reserve stores its Pyth oracle at byte 107 and its Switchboard oracle after it.
    assert_eq!(reserve[107..139], base58_decode(SOL_USD).unwrap()[..], "the reserve's Pyth oracle is the SOL/USD price account");
    let mut tx = vec![1u8];
    tx.extend([0u8; 64]);
    tx.extend([1, 0, 3, 5]);
    tx.extend(base58_decode(MAINNET_WALLET).unwrap());
    tx.extend(base58_decode(SAVE_SOL_RESERVE).unwrap());
    tx.extend(&reserve[107..139]);
    tx.extend(&reserve[139..171]);
    tx.extend(base58_decode(SAVE).unwrap());
    tx.extend([0u8; 32]);
    // One instruction: program 4, accounts 1 to 3, data [3] (RefreshReserve).
    tx.extend([1, 4, 3, 1, 2, 3, 1, 3]);
    base64_encode(&tx)
}

/// The source's close for each minute of `[from, to)`, read here with curl.
fn source_closes(from: u64, to: u64) -> Vec<(u64, f64)> {
    let body = json!({ "type": "candleSnapshot", "req": { "coin": "SOL", "interval": "1m", "startTime": from * 1000, "endTime": (to - 60) * 1000 } }).to_string();
    let out = Command::new("curl").args(["-sS", "-m", "30", "-X", "POST", "-H", "content-type: application/json", "--data", &body, "https://api.hyperliquid.xyz/info"]).output().expect("curl");
    let rows: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("the source answered no JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    let mut closes: Vec<(u64, f64)> = rows.as_array().expect("candles").iter().map(|r| (r["t"].as_u64().unwrap() / 1000, r["c"].as_str().unwrap().parse().unwrap())).collect();
    closes.sort_by_key(|c| c.0);
    closes
}

#[test]
#[ignore]
fn solana_price_set_and_replay() {
    let name = "acc-sol-px";
    chainsim(&["down", name]);
    let d = ok(&["describe"]);
    assert!(d["commands"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().contains("price <feed|price account> <usd>")));
    ok(&["up", "solana-mainnet", "--name", name]);

    // 1. A day later, a price set reads back with a publish time equal to the Clock.
    let before = price(name, SOL_USD);
    ok(&["time", name, "--advance", "86400"]);
    let stale = price(name, SOL_USD);
    assert_eq!(stale, before, "the account keeps its data when the clock moves");
    assert!(clock(name) - stale.publish_time >= 86400);

    // 2, control. A real program refuses the stale price.
    let refused = fails(&["send", name, "--as", MAINNET_WALLET, "--data", &refresh_reserve(name)], 1, "tx_reverted");
    assert!(refused["data"]["logs"].to_string().contains("PriceTooOld"), "{refused}");

    let set = ok(&["set", name, "price", "SOL/USD", "123.45", "--conf", "0.05"]);
    assert_eq!((set["feed"].as_str(), set["account"].as_str(), set["price"].as_str(), set["conf"].as_str()), (Some("SOL/USD"), Some(SOL_USD), Some("123.45"), Some("0.05")));
    assert_eq!((set["raw"].as_str(), set["exponent"].as_i64()), (Some("12345000000"), Some(-8)));
    assert_eq!(set["publishTime"], set["clock"]);
    assert!(set["note"].as_str().is_some_and(|n| n.contains("chainsim status")));
    let now = price(name, SOL_USD);
    assert_eq!((now.price, now.conf, now.exponent), (12_345_000_000, 5_000_000, -8));
    assert_eq!(Some(now.publish_time), set["clock"].as_i64());
    // The Clock itself, read a moment later, is within a few seconds of it.
    assert!((0..10).contains(&(clock(name) - now.publish_time)), "clock {} publish time {}", clock(name), now.publish_time);

    // 2. The same program call now succeeds, and the reserve holds the price that was set.
    let accepted = ok(&["send", name, "--as", MAINNET_WALLET, "--data", &refresh_reserve(name)]);
    assert_eq!(accepted["status"], "success");
    assert!(!accepted["logs"].to_string().contains("stale"), "{accepted}");
    let reserve = account(name, SAVE_SOL_RESERVE);
    let market_price = u128::from_le_bytes(reserve[211..227].try_into().unwrap());
    assert_eq!(market_price, 123_450_000_000_000_000_000, "the reserve's market price, 18 decimals");

    // Without --conf the account keeps its confidence. An address works as well as a symbol.
    let kept = ok(&["set", name, "price", SOL_USD, "99"]);
    assert_eq!((kept["feed"].as_str(), kept["conf"].as_str(), kept["previous"]["price"].as_str()), (Some("SOL/USD"), Some("0.05"), Some("123.45")));
    let btc = ok(&["set", name, "price", "btc/usd", "70000.5"]);
    assert_eq!((btc["account"].as_str(), btc["price"].as_str()), (Some(BTC_USD), Some("70000.5")));
    assert_eq!(price(name, BTC_USD).price, 7_000_050_000_000);
    // What set price refuses.
    fails(&["set", name, "price", "DOGE/USD", "1"], 2, "usage");
    fails(&["set", name, "price", "SOL/USD", "abc"], 2, "usage");
    fails(&["set", name, "price", "SOL/USD", "1.123456789"], 2, "usage");
    fails(&["set", name, "price", USDC, "1"], 3, "unsupported");

    // 3. A replay of one past hour of SOL/USD.
    let host_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let from = (host_now - 86400) / 60 * 60;
    let to = from + 3600;
    let started = clock(name);
    let replay = ok(&["time", name, "--replay", "SOL/USD", "--from", &from.to_string(), "--to", &to.to_string()]);
    let applied = replay["applied"].as_array().expect("applied steps");
    let source = source_closes(from, to);
    assert!(applied.len() >= 30, "{} steps", applied.len());
    assert_eq!(applied.len(), source.len(), "one step for each candle of the source");
    assert_eq!((replay["steps"].as_u64(), replay["feed"].as_str()), (Some(applied.len() as u64), Some("SOL/USD")));
    assert!(replay["source"].as_str().unwrap().contains("Hyperliquid"));
    // Three sampled steps: the price the account read back at that step is the source's for that minute,
    // and the fork's clock moved by the time between the candles.
    let first_clock = applied[0]["clock"].as_i64().unwrap();
    assert!((0..10).contains(&(first_clock - started)), "the first step lands at the fork's clock");
    for i in [0, applied.len() / 2, applied.len() - 1] {
        let (step, (time, close)) = (&applied[i], source[i]);
        assert_eq!(step["time"].as_u64(), Some(time), "step {i}");
        let got: f64 = step["price"].as_str().unwrap().parse().unwrap();
        assert!((got - close).abs() < 1e-9, "step {i}: the account read {got}, the source closed at {close}");
        // The Clock can tick once between the jump and the write.
        let late = step["clock"].as_i64().unwrap() - first_clock - (time - source[0].0) as i64;
        assert!((0..=2).contains(&late), "step {i}: the clock is {late} s off the candle offset");
    }
    // The account now holds the last step, published at the Clock.
    let last = applied.last().unwrap();
    let end = price(name, SOL_USD);
    assert!((end.price as f64 / 1e8 - source.last().unwrap().1).abs() < 1e-9, "{end:?}");
    assert_eq!(Some(end.publish_time), last["clock"].as_i64());
    assert!((0..=2).contains(&(replay["clock"]["to"].as_i64().unwrap() - last["clock"].as_i64().unwrap())));
    assert!((0..10).contains(&(clock(name) - end.publish_time)));
    // A program accepts the replayed price too.
    assert_eq!(ok(&["send", name, "--as", MAINNET_WALLET, "--data", &refresh_reserve(name)])["status"], "success");

    // What a replay refuses.
    fails(&["time", name, "--replay", "USDC/USD", "--from", &from.to_string(), "--to", &to.to_string()], 3, "unsupported");
    fails(&["time", name, "--replay", "SOL/USD", "--from", &to.to_string(), "--to", &from.to_string()], 2, "usage");
    // More than a day, wholly in the past.
    fails(&["time", name, "--replay", "SOL/USD", "--from", &(from - 3 * 86400).to_string(), "--to", &(from - 2 * 86400 + 60).to_string()], 2, "usage");
    fails(&["time", name, "--replay", "SOL/USD", "--from", &host_now.to_string(), "--to", &(host_now + 3600).to_string()], 2, "usage");
    fails(&["time", name, "--replay", "NOPE", "--from", &from.to_string(), "--to", &to.to_string()], 2, "usage");
    fails(&["time", name, "--replay", "SOL/USD"], 2, "usage");
    // A range the source no longer keeps points at --file.
    let old = fails(&["time", name, "--replay", "SOL/USD", "--from", "2025-01-01", "--to", "2025-01-01T01:00Z"], 2, "usage");
    assert!(old["error"]["next"].as_str().unwrap().contains("--file"), "{old}");

    // A price file replays any feed at its own times: here USDC/USD, 90 and then 210 seconds apart.
    let file = std::env::temp_dir().join("chainsim-acc-sol-px.csv");
    std::fs::write(&file, "time,price\n1735689600,1.0001\n1735689690,0.9995\n1735689900,1\n").unwrap();
    let before_file = clock(name);
    let played = ok(&["time", name, "--replay", "USDC/USD", "--file", file.to_str().unwrap()]);
    assert_eq!((played["steps"].as_u64(), played["feed"].as_str(), played["from"].as_u64(), played["to"].as_u64()), (Some(3), Some("USDC/USD"), Some(1735689600), Some(1735689900)));
    assert!(played["source"].as_str().unwrap().contains("CSV"));
    let rows = played["applied"].as_array().unwrap();
    let prices: Vec<&str> = rows.iter().map(|r| r["price"].as_str().unwrap()).collect();
    assert_eq!(prices, ["1.0001", "0.9995", "1"]);
    let at: Vec<i64> = rows.iter().map(|r| r["clock"].as_i64().unwrap()).collect();
    assert!((0..10).contains(&(at[0] - before_file)), "the first row lands at the fork's clock");
    assert!((90..=92).contains(&(at[1] - at[0])) && (300..=302).contains(&(at[2] - at[0])), "{at:?}");
    let usdc = price(name, USDC_USD);
    assert_eq!((usdc.price, usdc.publish_time), (100_000_000, at[2]));
    std::fs::write(&file, "1735689600,1\n1735689600,2\n").unwrap();
    fails(&["time", name, "--replay", "USDC/USD", "--file", file.to_str().unwrap()], 2, "usage");
    fails(&["time", name, "--replay", "USDC/USD", "--file", "/no/such/file.csv"], 2, "usage");
    let _ = std::fs::remove_file(&file);

    // status lists each price change.
    let status = ok(&["status", name]);
    let mods = status["modifications"].as_array().unwrap();
    let kinds: Vec<&str> = mods.iter().map(|m| m["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "set-price").count(), 3, "{kinds:?}");
    assert_eq!(kinds.iter().filter(|k| **k == "replay").count(), 2, "{kinds:?}");
    let listed = mods.iter().find(|m| m["kind"] == "replay").expect("a replay entry");
    assert_eq!((listed["feed"].as_str(), listed["steps"].as_u64(), listed["of"].as_u64(), listed["from"].as_u64()), (Some("SOL/USD"), Some(applied.len() as u64), Some(applied.len() as u64), Some(from)));

    assert_eq!(ok(&["down", name])["removed"], true);
    assert_gone(name);
}
