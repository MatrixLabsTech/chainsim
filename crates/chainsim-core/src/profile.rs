//! Profiles: data that binds a chain to an adapter, an engine, an upstream,
//! ports and fixes (design section 6). Embedded from `profiles/*.json`.

use serde::{Deserialize, Serialize};

use crate::embedded::{ASSETS, PROFILES};
use crate::error::{Error, ErrorCode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Maturity {
    Stable,
    Experimental,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// The public endpoint used when `env` is unset.
    pub default: String,
    /// The environment variable that may hold a provider URL. Its value never
    /// reaches a file, a log or `status`.
    pub env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    pub rpc: u16,
    pub ws: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSettings {
    /// The default fork point: `latest`.
    pub default: String,
    /// Engine flags such as the rate budget and retries.
    #[serde(default)]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// `code` (EVM `anvil_setCode`), `storage` or `account`.
    pub kind: String,
    pub target: String,
    /// A file under `profiles/assets/`.
    pub file: String,
    /// What a report must say about it.
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAsset {
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Token {
    pub symbol: String,
    pub address: String,
    pub decimals: u8,
    /// The token is a view of the native balance (Arc USDC), so funding it
    /// sets the native balance.
    #[serde(default)]
    pub native_view: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Warmup {
    /// A zero-value native transfer from a throwaway sender.
    NativeTransfer,
    /// `approve(spender, 0)` on a profile token.
    Approve { token: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelftestSettings {
    /// The profile token the ERC-20 checks use.
    pub token: Option<String>,
    /// Chain-specific extra checks, by name.
    #[serde(default)]
    pub extras: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The profile key, equal to the file stem: `arc-mainnet`.
    pub key: String,
    /// CAIP-2: `eip155:5042`.
    pub id: String,
    pub name: String,
    pub family: String,
    /// An engine ID from the engine lock.
    pub engine: String,
    pub upstream: Upstream,
    pub ports: Ports,
    pub maturity: Maturity,
    pub fork: ForkSettings,
    pub native: Option<NativeAsset>,
    #[serde(default)]
    pub overrides: Vec<Override>,
    #[serde(default)]
    pub tokens: Vec<Token>,
    #[serde(default)]
    pub warmup: Vec<Warmup>,
    pub selftest: SelftestSettings,
    /// Sui only: the seeded address that impersonation draws from.
    pub funder: Option<String>,
    /// The control sidecar's engine ID in the lock (default `control-curl`),
    /// for a family whose engine needs another client, such as grpcurl.
    pub control: Option<String>,
    pub limits: Vec<String>,
}

impl Profile {
    /// The upstream URL: the provider URL from the environment when set, else
    /// the public default. Never print the result.
    pub fn upstream_url(&self) -> String {
        std::env::var(&self.upstream.env).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| self.upstream.default.clone())
    }

    /// True when the upstream comes from the environment (a secret).
    pub fn upstream_is_secret(&self) -> bool {
        std::env::var(&self.upstream.env).is_ok_and(|v| !v.trim().is_empty())
    }

    /// The upstream as `describe` and the fork object show it.
    pub fn upstream_info(&self) -> crate::fork::UpstreamInfo {
        crate::fork::UpstreamInfo { default: self.upstream.default.clone(), env: self.upstream.env.clone(), set: self.upstream_is_secret() }
    }

    pub fn control_engine(&self) -> &str {
        self.control.as_deref().unwrap_or(crate::lock::CONTROL_ENGINE)
    }

    pub fn token(&self, symbol_or_address: &str) -> Option<&Token> {
        self.tokens.iter().find(|t| t.symbol.eq_ignore_ascii_case(symbol_or_address) || t.address.eq_ignore_ascii_case(symbol_or_address))
    }
}

fn parse(stem: &str, json: &str) -> Result<Profile> {
    let p: Profile = serde_json::from_str(json).map_err(|e| Error::new(ErrorCode::Internal, format!("profiles/{stem}.json: {e}")))?;
    if p.key != stem {
        return Err(Error::new(ErrorCode::Internal, format!("profiles/{stem}.json: key is {}", p.key)));
    }
    Ok(p)
}

/// Every embedded profile, sorted by key.
pub fn all() -> Result<Vec<Profile>> {
    PROFILES.iter().map(|(stem, json)| parse(stem, json)).collect()
}

/// One profile by key, or `unknown_chain` (exit 4).
pub fn get(key: &str) -> Result<Profile> {
    match PROFILES.iter().find(|(stem, _)| *stem == key) {
        Some((stem, json)) => parse(stem, json),
        None => {
            let keys: Vec<_> = PROFILES.iter().map(|(s, _)| *s).collect();
            Err(Error::new(ErrorCode::UnknownChain, format!("no profile {key}; profiles: {}", keys.join(", "))).next("chainsim describe"))
        }
    }
}

/// The contents of an override asset under `profiles/assets/`.
pub fn asset(file: &str) -> Result<&'static str> {
    ASSETS.iter().find(|(name, _)| *name == file).map(|(_, body)| *body).ok_or_else(|| Error::new(ErrorCode::Internal, format!("profiles/assets/{file} is not embedded")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_parses_and_names_a_locked_engine() {
        let lock = crate::lock::EngineLock::embedded().unwrap();
        let profiles = all().unwrap();
        assert!(!profiles.is_empty());
        for p in &profiles {
            assert!(lock.engines.contains_key(&p.engine), "{} names engine {} not in the lock", p.key, p.engine);
            assert!(p.id.contains(':'), "{} id is not CAIP-2", p.key);
            assert!(!p.limits.is_empty(), "{} has no limits", p.key);
            assert!(!p.upstream.env.is_empty());
            assert!(lock.engines.contains_key(p.control_engine()), "{} names control engine {} not in the lock", p.key, p.control_engine());
            for o in &p.overrides {
                asset(&o.file).unwrap();
            }
            if let Some(t) = &p.selftest.token {
                assert!(p.token(t).is_some(), "{} selftest token {t} is not a profile token", p.key);
            }
            for w in &p.warmup {
                if let Warmup::Approve { token } = w {
                    assert!(p.token(token).is_some(), "{} warm-up token {token} is not a profile token", p.key);
                }
            }
        }
    }

    #[test]
    fn unknown_profile_is_exit_4() {
        let e = get("no-such-chain").unwrap_err();
        assert_eq!(e.code.exit_code(), 4);
    }
}
