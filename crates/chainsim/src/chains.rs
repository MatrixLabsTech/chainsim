//! `chainsim chains`: every supported chain with its engine and the features
//! it supports, from the profiles, the engine lock and the adapters.

use chainsim_core::fork::SetKind;
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile};
use chainsim_core::Result;
use serde_json::{json, Value};

/// The operations an adapter family answers (the rest exit 3).
fn operations(family: &str) -> &'static [&'static str] {
    match family {
        "evm" => chainsim_evm::OPERATIONS,
        "sui" => chainsim_sui::OPERATIONS,
        "aptos" => chainsim_aptos::OPERATIONS,
        _ => &[],
    }
}

/// Whether `set <kind>` works on a profile, from its adapter.
fn supports_set(p: &Profile, kind: &SetKind) -> bool {
    match p.family.as_str() {
        "evm" => chainsim_evm::supports_set(p, kind),
        "sui" => chainsim_sui::supports_set(p, kind),
        "aptos" => chainsim_aptos::supports_set(p, kind),
        _ => false,
    }
}

/// The engine as `chains` and the README show it: its name and version.
pub fn engine_label(p: &Profile, lock: &EngineLock) -> String {
    match lock.engines.get(&p.engine) {
        Some(e) => format!("{} {}", e.name.as_deref().unwrap_or(&p.engine), e.version),
        None => p.engine.clone(),
    }
}

/// The feature rows, in table order: (key, label).
pub fn feature_rows() -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = [("up", "up"), ("fundNative", "fund native"), ("fundTokens", "fund tokens"), ("sendAs", "send --as")].iter().map(|(k, l)| (k.to_string(), l.to_string())).collect();
    for kind in SetKind::examples() {
        rows.push((format!("set.{}", kind.name()), format!("set {}", kind.name())));
    }
    rows.push(("set.hypercore".into(), "set hypercore".into()));
    for (k, l) in [("snapshotRevert", "snapshot, revert"), ("time", "time"), ("mine", "mine"), ("call", "call"), ("selftest", "selftest")] {
        rows.push((k.into(), l.into()));
    }
    rows
}

/// One chain's features, keyed as in [`feature_rows`].
fn features(p: &Profile) -> Value {
    let ops = operations(&p.family);
    let has = |op: &str| ops.contains(&op);
    let mut set = serde_json::Map::new();
    for kind in SetKind::examples() {
        set.insert(kind.name().to_string(), json!(has("set") && supports_set(p, &kind)));
    }
    // `set hypercore` encodes answers for `set precompile`.
    let precompile = set["precompile"].clone();
    set.insert("hypercore".into(), precompile);
    json!({
        "up": has("up"),
        "fundNative": has("fund") && p.native.is_some(),
        "fundTokens": has("fund") && !p.tokens.is_empty(),
        "sendAs": has("send"),
        "set": set,
        "snapshotRevert": has("snapshot") && has("revert"),
        "time": has("time"),
        "mine": has("mine"),
        "call": has("call"),
        "selftest": has("selftest"),
    })
}

/// What `chainsim chains` returns.
pub fn data() -> Result<Value> {
    let lock = EngineLock::embedded()?;
    let chains: Vec<Value> = profile::all()?
        .iter()
        .map(|p| json!({ "chain": p.key, "name": p.name, "family": p.family, "engine": engine_label(p, &lock), "maturity": p.maturity, "features": features(p) }))
        .collect();
    Ok(json!({ "chains": chains }))
}

fn lookup<'a>(features: &'a Value, key: &str) -> &'a Value {
    match key.split_once('.') {
        Some((a, b)) => &features[a][b],
        None => &features[key],
    }
}

/// The table `chainsim chains` prints in a terminal.
pub fn table(data: &Value) -> String {
    let chains = data["chains"].as_array().cloned().unwrap_or_default();
    let col = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut out = String::from("Supported chains\n\n");
    let w_key = chains.iter().map(|c| col(c, "chain").len()).max().unwrap_or(0);
    let w_name = chains.iter().map(|c| col(c, "name").len()).max().unwrap_or(0);
    for c in &chains {
        out.push_str(&format!("  {:<w_key$}  {:<w_name$}  {}\n", col(c, "chain"), col(c, "name"), col(c, "engine")));
    }
    let rows = feature_rows();
    let w_feat = rows.iter().map(|(_, l)| l.len()).max().unwrap_or(0).max("Feature".len());
    out.push_str(&format!("\n  {:<w_feat$}", "Feature"));
    for c in &chains {
        out.push_str(&format!("  {}", col(c, "chain")));
    }
    out.push('\n');
    for (key, label) in rows {
        let mut line = format!("  {label:<w_feat$}");
        for c in &chains {
            let mark = if lookup(&c["features"], &key) == &json!(true) { "yes" } else { "-" };
            line.push_str(&format!("  {:<w$}", mark, w = col(c, "chain").len()));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str("\nA feature marked - exits 3 on that chain. chainsim describe <chain> has the details.\n");
    out
}

/// The end of `chainsim --help`: the chain keys and where to read more.
pub fn epilog() -> String {
    let keys: Vec<String> = profile::all().map(|ps| ps.into_iter().map(|p| p.key).collect()).unwrap_or_default();
    format!("Supported chains: {}\nRun chainsim chains for each chain's engine and features.", keys.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain<'a>(d: &'a Value, key: &str) -> &'a Value {
        d["chains"].as_array().unwrap().iter().find(|c| c["chain"] == key).unwrap_or_else(|| panic!("no chain {key}"))
    }

    #[test]
    fn features_follow_the_adapters() {
        let d = data().unwrap();
        assert_eq!(d["chains"].as_array().unwrap().len(), profile::all().unwrap().len());
        let aptos = &chain(&d, "aptos-mainnet")["features"];
        assert_eq!((aptos["time"].clone(), aptos["mine"].clone(), aptos["set"]["resource"].clone()), (json!(true), json!(true), json!(true)));
        for key in ["ethereum-mainnet", "arc-mainnet", "robinhood-mainnet", "hyperevm-mainnet"] {
            let f = &chain(&d, key)["features"];
            assert_eq!((f["time"].clone(), f["mine"].clone(), f["set"]["storage"].clone(), f["set"]["token"].clone(), f["set"]["owner"].clone()), (json!(true), json!(true), json!(true), json!(true), json!(false)), "{key}");
        }
        assert_eq!(chain(&d, "sui-mainnet")["features"]["fundTokens"], true);
        assert_eq!(chain(&d, "sui-mainnet")["features"]["set"]["object"], true);
        for c in d["chains"].as_array().unwrap() {
            let want = c["chain"] == "hyperevm-mainnet";
            assert_eq!(c["features"]["set"]["precompile"], json!(want), "{}", c["chain"]);
            assert_eq!(c["features"]["set"]["hypercore"], json!(want), "{}", c["chain"]);
        }
        assert_eq!(chain(&d, "robinhood-mainnet")["engine"], "Foundry anvil v1.8.5");
    }

    #[test]
    fn the_table_has_a_row_per_feature_and_a_column_per_chain() {
        let d = data().unwrap();
        let t = table(&d);
        for c in d["chains"].as_array().unwrap() {
            assert!(t.contains(c["chain"].as_str().unwrap()));
            assert!(t.contains(c["engine"].as_str().unwrap()));
        }
        let header = t.lines().find(|l| l.trim_start().starts_with("Feature")).unwrap();
        assert_eq!(header.split_whitespace().count(), 1 + d["chains"].as_array().unwrap().len());
        let precompile = t.lines().find(|l| l.trim_start().starts_with("set precompile")).unwrap();
        assert_eq!(precompile.matches("yes").count(), 1);
        assert!(epilog().contains("hyperevm-mainnet") && epilog().contains("chainsim chains"));
    }

    /// The README's Supported chains table: (chain key, engine cell) per row.
    fn readme_rows(readme: &str) -> Vec<(String, String)> {
        let section = readme.split("## Supported chains").nth(1).expect("a Supported chains section").split("\n## ").next().unwrap();
        section
            .lines()
            .filter(|l| l.starts_with("| <img"))
            .map(|l| {
                let cells: Vec<&str> = l.split('|').map(str::trim).collect();
                let key = cells[1].split('`').nth(1).unwrap_or_else(|| panic!("no `chain key` in README row {l}")).to_string();
                (key, cells[2].to_string())
            })
            .collect()
    }

    fn readme_mismatches(readme: &str) -> Vec<String> {
        let d = data().unwrap();
        let rows = readme_rows(readme);
        let mut bad = Vec::new();
        let mut keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        let mut want: Vec<&str> = d["chains"].as_array().unwrap().iter().map(|c| c["chain"].as_str().unwrap()).collect();
        keys.sort();
        want.sort();
        if keys != want {
            bad.push(format!("README chains {keys:?}, chainsim chains {want:?}"));
        }
        for (key, engine) in &rows {
            if let Some(c) = d["chains"].as_array().unwrap().iter().find(|c| c["chain"] == key.as_str()) {
                let label = c["engine"].as_str().unwrap();
                if !engine.starts_with(label) {
                    bad.push(format!("README engine for {key} is {engine:?}, chainsim chains says {label:?}"));
                }
            }
        }
        bad
    }

    #[test]
    fn the_readme_supported_chains_match_chainsim_chains() {
        let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md")).unwrap();
        assert_eq!(readme_mismatches(&readme), Vec::<String>::new());
        // The check bites: a changed engine string or a missing row fails it.
        let changed = readme.replacen("Foundry anvil v1.8.5 with HyperCore", "Foundry anvil v1.8.6 with HyperCore", 1);
        assert_ne!(changed, readme);
        assert_eq!(readme_mismatches(&changed).len(), 1);
        let dropped: String = readme.lines().filter(|l| !l.contains("`aptos-mainnet`")).collect::<Vec<_>>().join("\n");
        assert!(readme_mismatches(&dropped)[0].starts_with("README chains"));
    }
}
