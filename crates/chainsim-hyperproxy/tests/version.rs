//! The proxy's engine version in engines.lock.json is the workspace version
//! plus a hash of every build input of the proxy binary, so changing one of
//! them without a new version (and a new release) fails here.
//!
//! The inputs are every file of the crates the proxy compiles (tests aside),
//! the workspace manifest, Cargo.lock and the build Dockerfile with its
//! toolchain pin. chainsim-core also embeds profiles/, engines/ and
//! engines.lock.json, data the proxy never reads, and the lock holds this
//! version itself, so those stay out.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Folders hashed whole: the proxy crate and the crates it depends on.
const CRATES: &[&str] = &["crates/chainsim-hyperproxy", "crates/chainsim-evm", "crates/chainsim-core"];
/// Single files hashed.
const FILES: &[&str] = &["Cargo.toml", "Cargo.lock", "engines/hyperevm-proxy/build.Dockerfile"];
/// Embedded data the proxy binary carries but never reads.
const DATA: &[&str] = &["profiles", "engines", "engines.lock.json"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Every file under `dir`, relative to the root, sorted, without dot files or tests/ folders.
fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = e.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            if name != "tests" {
                walk(root, &path, out);
            }
        } else {
            out.insert(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

fn inputs(root: &Path) -> BTreeSet<String> {
    let mut files: BTreeSet<String> = FILES.iter().map(|f| f.to_string()).collect();
    for c in CRATES {
        walk(root, &root.join(c), &mut files);
    }
    files
}

fn source_hash(root: &Path) -> String {
    let mut all = Vec::new();
    for f in inputs(root) {
        all.extend(f.as_bytes());
        all.push(0);
        // Line endings as committed, whatever the checkout did.
        let body = std::fs::read(root.join(&f)).unwrap_or_else(|e| panic!("read {f}: {e}"));
        all.extend(body.iter().filter(|b| **b != b'\r'));
        all.push(0);
    }
    let h = alloy_primitives::keccak256(&all);
    alloy_primitives::hex::encode(&h[..6])
}

fn hashed(rel: &str) -> bool {
    CRATES.iter().any(|c| rel == *c || rel.starts_with(&format!("{c}/"))) || FILES.contains(&rel)
}

fn data(rel: &str) -> bool {
    DATA.iter().any(|d| rel == *d || rel.starts_with(&format!("{d}/")))
}

#[test]
fn the_lock_version_matches_the_proxy_build_inputs() {
    let root = root();
    let lock: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("engines.lock.json")).unwrap()).unwrap();
    let entry = &lock["engines"]["hyperevm-proxy"];
    let want = format!("{}-{}", env!("CARGO_PKG_VERSION"), source_hash(&root));
    assert_eq!(entry["version"], want, "the proxy build inputs changed: set the hyperevm-proxy version, image tag and asset names in engines.lock.json to {want}, then publish its release");
    assert_eq!(entry["build"]["image"], format!("chainsim/hyperevm-proxy:{want}"));
    for arch in ["amd64", "arm64"] {
        let url = entry["build"]["tarballs"][arch]["url"].as_str().unwrap();
        assert!(url.ends_with(&format!("/hyperevm-proxy-{want}/hyperevm-proxy-{want}-linux-{arch}.tar.gz")), "{url}");
    }
}

/// The workspace path of each dependency a manifest takes from the workspace or by path.
fn path_deps(root: &Path, manifest: &str) -> Vec<String> {
    let workspace = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let ws_path = |name: &str| {
        workspace.lines().find(|l| l.trim_start().starts_with(&format!("{name} ")) && l.contains("path =")).and_then(|l| l.split("path = \"").nth(1)).and_then(|r| r.split('"').next()).map(str::to_string)
    };
    let dir = Path::new(manifest).parent().unwrap();
    let mut deps = Vec::new();
    for line in std::fs::read_to_string(root.join(manifest)).unwrap().lines() {
        let line = line.trim();
        if let Some(rest) = line.split("path = \"").nth(1) {
            let rel = dir.join(rest.split('"').next().unwrap());
            deps.push(root.join(rel).canonicalize().unwrap().strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        } else if line.contains("workspace = true") {
            let name = line.split(['.', ' ', '=']).next().unwrap();
            deps.extend(ws_path(name));
        }
    }
    deps
}

#[test]
fn every_build_input_of_the_proxy_is_hashed() {
    let root = root();
    // The crates the proxy compiles, followed through their path dependencies.
    let mut seen = BTreeSet::new();
    let mut todo = vec!["crates/chainsim-hyperproxy".to_string()];
    while let Some(c) = todo.pop() {
        if !seen.insert(c.clone()) {
            continue;
        }
        assert!(hashed(&c), "the proxy compiles {c}, which the version hash leaves out: add it to CRATES");
        todo.extend(path_deps(&root, &format!("{c}/Cargo.toml")));
    }
    // Files the compiled sources and build scripts pull in.
    for f in inputs(&root).iter().filter(|f| f.ends_with(".rs")) {
        let text = std::fs::read_to_string(root.join(f)).unwrap();
        let dir = Path::new(f).parent().unwrap();
        for m in ["include_str!(\"", "include_bytes!(\""] {
            for rest in text.split(m).skip(1) {
                let target = rest.split('"').next().unwrap();
                if target.starts_with('/') || target.contains('{') {
                    continue;
                }
                let rel = root.join(dir).join(target).canonicalize().unwrap_or_else(|e| panic!("{f} includes {target}: {e}"));
                let rel = rel.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                assert!(hashed(&rel) || data(&rel), "{f} includes {rel}, which the version hash leaves out");
            }
        }
        if f.ends_with("build.rs") {
            for rest in text.split("root.join(\"").skip(1) {
                let target = rest.split('"').next().unwrap();
                assert!(hashed(target) || data(target), "{f} reads {target}, which the version hash leaves out");
            }
        }
    }
    // The listed files exist, so a rename fails here and not in the release.
    for f in FILES.iter().chain(CRATES) {
        assert!(root.join(f).exists(), "{f} is listed as a proxy build input but does not exist");
    }
    assert!(seen.contains("crates/chainsim-core") && seen.contains("crates/chainsim-evm"));
}
