// Embeds every profiles/*.json and the engine lock, so adding a chain is a new
// profile file and no code change (design section 6).
use std::{env, fs, path::Path};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let profiles = root.join("profiles");
    println!("cargo:rerun-if-changed={}", profiles.display());
    println!("cargo:rerun-if-changed={}", root.join("engines.lock.json").display());

    let mut entries: Vec<_> = fs::read_dir(&profiles)
        .expect("profiles/ exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    entries.sort();

    let mut out = String::from("/// Embedded profiles: (file stem, JSON).\npub static PROFILES: &[(&str, &str)] = &[\n");
    for path in &entries {
        println!("cargo:rerun-if-changed={}", path.display());
        let stem = path.file_stem().unwrap().to_string_lossy();
        let abs = fs::canonicalize(path).unwrap();
        out.push_str(&format!("    ({stem:?}, include_str!({:?})),\n", abs.display().to_string()));
    }
    out.push_str("];\n");
    let lock = fs::canonicalize(root.join("engines.lock.json")).expect("engines.lock.json exists");
    out.push_str(&format!("/// The embedded engine lock.\npub static ENGINE_LOCK: &str = include_str!({:?});\n", lock.display().to_string()));

    let assets = fs::canonicalize(profiles.join("assets")).expect("profiles/assets exists");
    println!("cargo:rerun-if-changed={}", assets.display());
    let mut files: Vec<_> = fs::read_dir(&assets).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    out.push_str("/// Embedded override assets: (file name, contents).\npub static ASSETS: &[(&str, &str)] = &[\n");
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        out.push_str(&format!("    ({name:?}, include_str!({:?})),\n", path.display().to_string()));
    }
    out.push_str("];\n");

    let engines = root.join("engines");
    println!("cargo:rerun-if-changed={}", engines.display());
    let mut dirs: Vec<_> = fs::read_dir(&engines).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("Dockerfile").is_file()).collect()).unwrap_or_default();
    dirs.sort();
    out.push_str("/// Embedded engine build contexts: (folder under engines/, Dockerfile).\npub static DOCKERFILES: &[(&str, &str)] = &[\n");
    for dir in &dirs {
        let file = fs::canonicalize(dir.join("Dockerfile")).unwrap();
        println!("cargo:rerun-if-changed={}", file.display());
        let name = dir.file_name().unwrap().to_string_lossy();
        out.push_str(&format!("    ({name:?}, include_str!({:?})),\n", file.display().to_string()));
    }
    out.push_str("];\n");

    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("embedded.rs"), out).unwrap();
}
