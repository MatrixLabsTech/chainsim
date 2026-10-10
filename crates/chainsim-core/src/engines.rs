//! What `chainsim engines` lists and what `chainsim prune` removes (design
//! section 7), as pure functions over the lock, the profiles and what Docker
//! holds, so both are testable without Docker.
//!
//! Every engine image chainsim pulls or builds carries a `chainsim/<id>:<version>`
//! tag. That tag is how prune tells chainsim's images from everyone else's: it
//! never touches an image without one.

use std::collections::{BTreeSet, HashSet};

use serde::Serialize;

use crate::lock::EngineLock;
use crate::profile::Profile;

/// The repository prefix of chainsim's own image tags.
pub const OWNED_REPO: &str = "chainsim/";

/// The tag chainsim gives an engine image.
pub fn owned_tag(id: &str, version: &str) -> String {
    format!("{OWNED_REPO}{id}:{version}")
}

/// One lock entry, as `chainsim engines` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineRow {
    pub engine: String,
    pub version: String,
    /// `pull` or `build`.
    pub source: &'static str,
    /// The image this host runs: pinned by digest, or the built tag.
    pub image: String,
    pub present: bool,
    /// The profiles that run it, as their engine or their control sidecar.
    pub used_by: Vec<String>,
}

/// The profiles that use each engine.
fn users(profiles: &[Profile], id: &str) -> Vec<String> {
    profiles.iter().filter(|p| p.engine == id || p.control_engine() == id).map(|p| p.key.clone()).collect()
}

pub fn rows(lock: &EngineLock, profiles: &[Profile], present: impl Fn(&str) -> bool) -> Vec<EngineRow> {
    lock.engines
        .iter()
        .map(|(id, e)| {
            let image = e.reference();
            EngineRow {
                engine: id.clone(),
                version: e.version.clone(),
                source: if e.pull.is_some() { "pull" } else { "build" },
                present: present(&image),
                image,
                used_by: users(profiles, id),
            }
        })
        .collect()
}

/// What `prune` removes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PrunePlan {
    /// Image tags or references to remove, without force.
    pub images: Vec<String>,
    pub volumes: Vec<String>,
}

/// Plans a prune.
/// - `owned_tags`: the local `chainsim/*` image tags.
/// - `present`: whether a lock engine's image is on this host.
/// - `volumes`: local volumes with their `org.chainsim.fork` label.
/// - `live_forks`: forks that still have a container of any role.
pub fn prune_plan(lock: &EngineLock, profiles: &[Profile], owned_tags: &[String], present: impl Fn(&str) -> bool, volumes: &[(String, String)], live_forks: &HashSet<String>) -> PrunePlan {
    let mut keep = HashSet::new();
    let mut images = BTreeSet::new();
    for (id, e) in &lock.engines {
        let tag = owned_tag(id, &e.version);
        if users(profiles, id).is_empty() {
            // A lock engine no profile runs: its image goes too.
            if present(&e.reference()) {
                images.insert(e.reference());
            }
        } else {
            keep.insert(tag);
        }
    }
    for tag in owned_tags {
        if tag.starts_with(OWNED_REPO) && !keep.contains(tag) {
            images.insert(tag.clone());
        }
    }
    let volumes = volumes.iter().filter(|(_, fork)| !live_forks.contains(fork)).map(|(v, _)| v.clone()).collect::<BTreeSet<_>>();
    PrunePlan { images: images.into_iter().collect(), volumes: volumes.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (EngineLock, Vec<Profile>) {
        (EngineLock::embedded().unwrap(), crate::profile::all().unwrap())
    }

    #[test]
    fn rows_list_every_lock_engine_with_its_users() {
        let (lock, profiles) = fixture();
        let rows = rows(&lock, &profiles, |image| image.contains("foundry:v1.8.5"));
        assert_eq!(rows.len(), lock.engines.len());
        let foundry = rows.iter().find(|r| r.engine == "foundry").unwrap();
        assert!(foundry.present && foundry.source == "pull");
        assert_eq!(foundry.used_by, ["ethereum-mainnet", "robinhood-mainnet"]);
        let curl = rows.iter().find(|r| r.engine == "control-curl").unwrap();
        assert!(curl.used_by.contains(&"arc-mainnet".to_string()), "control sidecars count as users");
        assert_eq!(rows.iter().find(|r| r.engine == "arc-foundry").unwrap().source, "build");
    }

    #[test]
    fn prune_keeps_current_engines_and_live_volumes() {
        let (lock, profiles) = fixture();
        let foundry = &lock.engines["foundry"];
        let tags = vec![
            owned_tag("foundry", &foundry.version),
            owned_tag("foundry", "v1.0.0"),
            owned_tag("retired-engine", "v1"),
            "someone/else:latest".to_string(),
        ];
        let volumes = vec![("chainsim-gone-data".to_string(), "gone".to_string()), ("chainsim-live-data".to_string(), "live".to_string())];
        let live = HashSet::from(["live".to_string()]);
        let plan = prune_plan(&lock, &profiles, &tags, |_| true, &volumes, &live);
        assert_eq!(plan.images, [owned_tag("foundry", "v1.0.0"), owned_tag("retired-engine", "v1")]);
        assert_eq!(plan.volumes, ["chainsim-gone-data"]);
    }

    #[test]
    fn prune_removes_a_lock_engine_no_profile_runs() {
        let (mut lock, profiles) = fixture();
        let mut spare = lock.engines["foundry"].clone();
        spare.version = "v9".into();
        let reference = spare.reference();
        lock.engines.insert("spare".into(), spare);
        let plan = prune_plan(&lock, &profiles, &[owned_tag("spare", "v9")], |r| r == reference, &[], &HashSet::new());
        assert_eq!(plan.images, [owned_tag("spare", "v9"), reference]);
    }
}
