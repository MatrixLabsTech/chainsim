//! Contract checks that need no Docker: describe's upstream field.

use std::process::Command;

use serde_json::Value;

fn describe(chain: &str, env: Option<&str>) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chainsim"));
    cmd.args(["describe", chain, "--json"]).env_remove("CHAINSIM_ARC_MAINNET_RPC");
    if let Some(v) = env {
        cmd.env("CHAINSIM_ARC_MAINNET_RPC", v);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn describe_shows_the_upstream_variable_and_never_its_value() {
    let v = describe("arc-mainnet", None);
    assert_eq!(v["data"]["upstream"], serde_json::json!({ "default": "https://rpc.mainnet.arc.io", "env": "CHAINSIM_ARC_MAINNET_RPC", "set": false }));

    let secret = "https://provider.example/v2/SECRETKEY";
    let v = describe("arc-mainnet", Some(secret));
    assert_eq!(v["data"]["upstream"]["set"], true);
    assert!(!v.to_string().contains("SECRETKEY"));
}
