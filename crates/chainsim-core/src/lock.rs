//! The engine lock (design section 7): each engine is a pinned pull, or a
//! local build from a checksummed tarball.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::embedded::ENGINE_LOCK;
use crate::error::{Error, ErrorCode, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineLock {
    pub engines: BTreeMap<String, Engine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub version: String,
    /// The user the engine runs as, when the image allows a non-root user.
    pub user: Option<String>,
    /// A writable directory the engine needs, mounted as tmpfs.
    pub home: Option<String>,
    pub pull: Option<Pull>,
    pub build: Option<Build>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pull {
    /// `repo:tag`. Never `latest`.
    pub image: String,
    /// The multi-arch index digest; the reference run is `image@digest`.
    pub digest: String,
    /// Per-architecture manifest digests, for reference.
    #[serde(default)]
    pub platforms: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The local tag the build produces.
    pub image: String,
    /// The folder under `engines/` holding the Dockerfile.
    pub context: String,
    /// The base image, pinned by digest.
    pub base: String,
    /// Per architecture (`amd64`, `arm64`): the tarball and its checksum.
    pub tarballs: BTreeMap<String, Tarball>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tarball {
    pub url: String,
    /// The published `.sha256` URL, checked against `sha256` at build time.
    pub sha256_url: String,
    pub sha256: String,
}

impl EngineLock {
    pub fn embedded() -> Result<Self> {
        serde_json::from_str(ENGINE_LOCK).map_err(|e| Error::new(ErrorCode::Internal, format!("engines.lock.json: {e}")))
    }

    pub fn engine(&self, id: &str) -> Result<&Engine> {
        self.engines.get(id).ok_or_else(|| Error::new(ErrorCode::Internal, format!("engine {id} is not in the lock")))
    }
}

impl Engine {
    /// The image reference a container runs.
    pub fn reference(&self) -> String {
        match (&self.pull, &self.build) {
            (Some(p), _) => format!("{}@{}", p.image, p.digest),
            (None, Some(b)) => b.image.clone(),
            (None, None) => String::new(),
        }
    }
}

/// The control sidecar's engine ID in the lock.
pub const CONTROL_ENGINE: &str = "control-curl";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_parses_and_pins_every_engine() {
        let lock = EngineLock::embedded().unwrap();
        assert!(lock.engines.contains_key(CONTROL_ENGINE));
        for (id, e) in &lock.engines {
            assert!(e.pull.is_some() != e.build.is_some(), "{id}: exactly one of pull or build");
            if let Some(p) = &e.pull {
                assert!(!p.image.ends_with(":latest") && p.image.contains(':'), "{id}: tag must be pinned");
                assert!(p.digest.starts_with("sha256:") && p.digest.len() == 71, "{id}: digest");
            }
            if let Some(b) = &e.build {
                assert!(b.base.contains("@sha256:"), "{id}: base must be pinned by digest");
                for (arch, t) in &b.tarballs {
                    assert!(arch == "amd64" || arch == "arm64", "{id}: arch {arch}");
                    assert_eq!(t.sha256.len(), 64, "{id}/{arch}: sha256");
                    assert!(t.url.starts_with("https://") && t.sha256_url.starts_with("https://"));
                }
            }
        }
    }
}
