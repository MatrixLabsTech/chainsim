//! The engine lock (design section 7): each engine is a pinned pull, or a
//! local build from a checksummed tarball or from source at a pinned commit.

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
    /// The multi-arch index digest; the reference run is `image@digest`
    /// unless `platforms` pins the host architecture.
    pub digest: String,
    /// Per architecture (`amd64`, `arm64`). A full `repo:tag@sha256:…`
    /// value is the reference run on that architecture, for images published
    /// as single-arch tags with no multi-arch index (mysten/sui-tools). A bare
    /// `sha256:…` value only records the manifest digest inside the index.
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
    /// The base image, pinned by digest, when it is the same on every architecture.
    #[serde(default)]
    pub base: Option<String>,
    /// Per architecture (`amd64`, `arm64`): the base image pinned by digest,
    /// for bases published as single-arch tags (mysten/sui-tools).
    #[serde(default)]
    pub bases: BTreeMap<String, String>,
    /// Per architecture (`amd64`, `arm64`): the tarball and its checksum.
    #[serde(default)]
    pub tarballs: BTreeMap<String, Tarball>,
    /// A build from source at a pinned commit, instead of a tarball.
    pub source: Option<Source>,
}

impl Build {
    /// The base image for an architecture, or None when the lock has none for it.
    pub fn base_for(&self, arch: &str) -> Option<&str> {
        self.bases.get(arch).or(self.base.as_ref()).map(String::as_str)
    }
}

/// A source build: the repo and commit to compile, and the toolchain image.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The git repository, fetched over https.
    pub repo: String,
    /// The branch that holds `commit`, for people; the build fetches `commit` only.
    pub branch: String,
    /// The full commit sha the build checks out and verifies.
    pub commit: String,
    /// The upstream release the branch is based on, and its commit.
    pub upstream: String,
    pub upstream_commit: String,
    /// The toolchain image, pinned by digest.
    pub toolchain: String,
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
    /// The image reference a container runs on this host.
    pub fn reference(&self) -> String {
        self.reference_for(host_arch())
    }

    /// The image reference for an architecture (`amd64` or `arm64`): its
    /// platform pin when the lock has one, else `image@digest`.
    pub fn reference_for(&self, arch: &str) -> String {
        match (&self.pull, &self.build) {
            (Some(p), _) => match p.platforms.get(arch).filter(|v| v.contains('@')) {
                Some(pinned) => pinned.clone(),
                None => format!("{}@{}", p.image, p.digest),
            },
            (None, Some(b)) => b.image.clone(),
            (None, None) => String::new(),
        }
    }
}

/// The host architecture in Docker's naming: `arm64` or `amd64`.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
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
                for (arch, v) in &p.platforms {
                    assert!(arch == "amd64" || arch == "arm64", "{id}: arch {arch}");
                    let digest = v.rsplit_once('@').map_or(v.as_str(), |(image, d)| {
                        assert!(image.contains(':') && !image.ends_with(":latest"), "{id}/{arch}: tag must be pinned");
                        d
                    });
                    assert!(digest.starts_with("sha256:") && digest.len() == 71, "{id}/{arch}: digest");
                }
            }
            if let Some(b) = &e.build {
                assert!(b.tarballs.is_empty() != b.source.is_none(), "{id}: exactly one of tarballs or source");
                let archs: Vec<&String> = if b.source.is_some() { b.bases.keys().collect() } else { b.tarballs.keys().collect() };
                assert!(!archs.is_empty(), "{id}: no architecture");
                for arch in archs {
                    assert!(arch == "amd64" || arch == "arm64", "{id}: arch {arch}");
                    assert!(b.base_for(arch).is_some_and(|base| base.contains("@sha256:")), "{id}/{arch}: base must be pinned by digest");
                }
                for (arch, t) in &b.tarballs {
                    assert_eq!(t.sha256.len(), 64, "{id}/{arch}: sha256");
                    assert!(t.url.starts_with("https://") && t.sha256_url.starts_with("https://"));
                }
                if let Some(s) = &b.source {
                    assert!(s.repo.starts_with("https://"), "{id}: repo over https");
                    for sha in [&s.commit, &s.upstream_commit] {
                        assert!(sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()), "{id}: full commit sha, not a branch name");
                    }
                    assert!(s.toolchain.contains("@sha256:"), "{id}: toolchain must be pinned by digest");
                    assert!(e.version.ends_with(&s.commit[..12]), "{id}: version names the commit, so a new commit builds a new image");
                }
                assert!(b.image.ends_with(&format!(":{}", e.version)), "{id}: image tag is the version");
            }
        }
    }

    fn pull(platforms: &[(&str, &str)]) -> Engine {
        Engine {
            version: "v1".into(),
            user: None,
            home: None,
            pull: Some(Pull {
                image: "repo/x:v1".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                platforms: platforms.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            }),
            build: None,
        }
    }

    #[test]
    fn platform_pins_pick_the_host_architecture() {
        let index = format!("repo/x:v1@sha256:{}", "a".repeat(64));
        let amd = format!("repo/x:v1@sha256:{}", "b".repeat(64));
        let arm = format!("repo/x:v1-arm64@sha256:{}", "c".repeat(64));
        let e = pull(&[("amd64", &amd), ("arm64", &arm)]);
        assert_eq!(e.reference_for("arm64"), arm);
        assert_eq!(e.reference_for("amd64"), amd);
        // No map: image@digest, as before.
        assert_eq!(pull(&[]).reference_for("arm64"), index);
        // A bare digest records the manifest inside the index and is not run.
        let bare = format!("sha256:{}", "d".repeat(64));
        assert_eq!(pull(&[("arm64", &bare)]).reference_for("arm64"), index);
        // An architecture missing from the map falls back too.
        assert_eq!(pull(&[("amd64", &amd)]).reference_for("arm64"), index);
    }
}
