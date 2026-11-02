//! The proxy's engine version in engines.lock.json is the workspace version
//! plus a hash of the sources the proxy binary is built from, so changing
//! them without a new version (and a new release) fails here.

use std::path::Path;

/// The files the proxy's behavior comes from, relative to the repository root.
const SOURCES: &[&str] = &[
    "crates/chainsim-hyperproxy/Cargo.toml",
    "crates/chainsim-hyperproxy/src/main.rs",
    "crates/chainsim-hyperproxy/src/lease.rs",
    "crates/chainsim-evm/src/core.rs",
    "crates/chainsim-evm/src/coresync.rs",
    "crates/chainsim-evm/src/hypercore.rs",
    "crates/chainsim-evm/src/units.rs",
];

fn source_hash(root: &Path) -> String {
    let mut all = Vec::new();
    for f in SOURCES {
        all.extend(f.as_bytes());
        all.push(0);
        all.extend(std::fs::read(root.join(f)).unwrap_or_else(|e| panic!("read {f}: {e}")));
        all.push(0);
    }
    let h = alloy_primitives::keccak256(&all);
    alloy_primitives::hex::encode(&h[..6])
}

#[test]
fn the_lock_version_matches_the_proxy_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("engines.lock.json")).unwrap()).unwrap();
    let entry = &lock["engines"]["hyperevm-proxy"];
    let want = format!("{}-{}", env!("CARGO_PKG_VERSION"), source_hash(&root));
    assert_eq!(entry["version"], want, "the proxy sources changed: set the hyperevm-proxy version, image tag and asset names in engines.lock.json to {want}, then publish its release");
    assert_eq!(entry["build"]["image"], format!("chainsim/hyperevm-proxy:{want}"));
    for arch in ["amd64", "arm64"] {
        let url = entry["build"]["tarballs"][arch]["url"].as_str().unwrap();
        assert!(url.ends_with(&format!("/hyperevm-proxy-{want}/hyperevm-proxy-{want}-linux-{arch}.tar.gz")), "{url}");
    }
}
