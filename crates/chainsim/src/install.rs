//! `chainsim install`: writes the embedded plugin to a local folder and adds
//! it as a local-path marketplace to Claude Code and Codex (design section 9).
//! The repo is private, so a GitHub marketplace would need git credentials on
//! every host; the binary carries the plugin instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

/// The marketplace and plugin name, in both CLIs.
const NAME: &str = "chainsim";
const PLUGIN: &str = "chainsim@chainsim";
/// The fallback when a CLI refuses the local folder.
const GITHUB: &str = "Lucklyric/chainsim";

/// The plugin tree, as published under `plugins/chainsim/`, plus the root marketplace.
pub const FILES: &[(&str, &str)] = &[
    (".claude-plugin/marketplace.json", include_str!("../../../.claude-plugin/marketplace.json")),
    ("plugins/chainsim/.claude-plugin/plugin.json", include_str!("../../../plugins/chainsim/.claude-plugin/plugin.json")),
    ("plugins/chainsim/skills/chainsim/SKILL.md", include_str!("../../../plugins/chainsim/skills/chainsim/SKILL.md")),
];

/// The folder `install` writes to: `--dir`, else `$CHAINSIM_HOME/marketplace`,
/// else `~/.chainsim/marketplace`.
pub fn default_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CHAINSIM_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join("marketplace"));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).ok_or_else(|| Error::new(ErrorCode::Usage, "HOME is not set").next("chainsim install --dir <folder>"))?;
    Ok(PathBuf::from(home).join(".chainsim").join("marketplace"))
}

/// Writes the embedded plugin under `dir`, replacing an earlier copy.
pub fn write_plugin(dir: &Path) -> Result<()> {
    let io = |e: std::io::Error| Error::new(ErrorCode::Internal, format!("write the plugin to {}: {e}", dir.display()));
    let _ = std::fs::remove_dir_all(dir.join("plugins").join(NAME));
    for (path, contents) in FILES {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().expect("plugin files sit in folders")).map_err(io)?;
        std::fs::write(&file, contents).map_err(io)?;
    }
    Ok(())
}

/// The permission rule `install` prints. It never edits settings itself.
pub fn permissions() -> Value {
    json!({
        "claude": {
            "file": "~/.claude/settings.json",
            "permissions": { "allow": ["Bash(chainsim *)"], "ask": ["Bash(chainsim down --all)", "Bash(chainsim down --all *)", "Bash(chainsim prune)", "Bash(chainsim prune *)"] },
        },
        "codex": "no per-command rule: Codex asks according to its approval policy",
        "note": "Add the claude rule to your settings to run chainsim without a prompt each time; `chainsim down --all` and `chainsim prune` still ask.",
    })
}

fn on_path(cli: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|d| d.join(cli).is_file())
}

/// Runs one CLI step. Its output goes to stderr; a failure returns its last line.
fn run(cli: &str, args: &[&str]) -> std::result::Result<(), String> {
    let out = Command::new(cli).args(args).stdin(Stdio::null()).output().map_err(|e| format!("{cli}: {e}"))?;
    eprint!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        return Ok(());
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Err(format!("`{cli} {}` failed: {}", args.join(" "), text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output").trim()))
}

/// Adds the folder as a marketplace and installs the plugin from it. Running
/// it again refreshes both, so it always ends on the binary's copy.
fn register(cli: &str, dir: &str) -> std::result::Result<(), String> {
    match cli {
        "claude" => {
            let _ = run(cli, &["plugin", "marketplace", "add", dir]); // fails harmlessly when already added
            run(cli, &["plugin", "marketplace", "update", NAME])?;
            run(cli, &["plugin", "install", PLUGIN])?;
            run(cli, &["plugin", "update", PLUGIN])
        }
        _ => {
            let _ = run(cli, &["plugin", "marketplace", "add", dir]);
            // Codex keeps a cached copy of the plugin: add again to refresh it.
            run(cli, &["plugin", "add", PLUGIN])
        }
    }
}

pub fn install(dir: Option<PathBuf>) -> Result<Value> {
    let dir = match dir {
        Some(d) => d,
        None => default_dir()?,
    };
    write_plugin(&dir)?;
    let dir_str = dir.to_string_lossy().to_string();
    let mut clis = serde_json::Map::new();
    let mut failed = Vec::new();
    for cli in ["claude", "codex"] {
        let entry = if !on_path(cli) {
            json!({ "installed": false, "skipped": format!("{cli} is not on PATH") })
        } else {
            match register(cli, &dir_str) {
                Ok(()) => json!({ "installed": true, "plugin": PLUGIN }),
                Err(reason) => {
                    failed.push(cli);
                    json!({ "installed": false, "reason": reason, "fallback": format!("{cli} plugin marketplace add {GITHUB}") })
                }
            }
        };
        clis.insert(cli.to_string(), entry);
    }
    let data = json!({ "dir": dir_str, "clis": clis, "permissions": permissions() });
    if !failed.is_empty() {
        let first = failed[0];
        return Err(Error::new(ErrorCode::Internal, format!("the plugin did not install in {}", failed.join(" and ")))
            .next(format!("{first} plugin marketplace add {GITHUB}, the GitHub install (needs git access to the repo)"))
            .data(data));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_plugin_is_one_skill_named_chainsim() {
        let skill = FILES.iter().find(|(p, _)| p.ends_with("SKILL.md")).unwrap().1;
        assert!(skill.starts_with("---\nname: chainsim\ndescription: Local mainnet forks of Ethereum, Arc, Robinhood Chain, Solana and Sui."));
        for word in ["chainsim describe", "chainsim up", "chainsim selftest", "chainsim fund", "chainsim snapshot", "chainsim revert", "chainsim down", "error.next", "`fork`"] {
            assert!(skill.contains(word), "{word}");
        }
        let market: Value = serde_json::from_str(FILES[0].1).unwrap();
        let plugin: Value = serde_json::from_str(FILES[1].1).unwrap();
        assert_eq!(market["plugins"][0]["source"], "./plugins/chainsim");
        assert_eq!((market["name"].as_str(), plugin["name"].as_str()), (Some(NAME), Some(NAME)));
        assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn write_plugin_lays_out_a_marketplace() {
        let dir = std::env::temp_dir().join(format!("chainsim-install-{}", std::process::id()));
        write_plugin(&dir).unwrap();
        write_plugin(&dir).unwrap();
        for (path, contents) in FILES {
            assert_eq!(std::fs::read_to_string(dir.join(path)).unwrap(), *contents);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
