//! The `error.code` list and the exit codes of the agent contract (design section 9).

use serde::Serialize;
use serde_json::Value;

/// Every `error.code` chainsim emits. An agent branches on the exit code, and
/// reads the code for detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Exit 1: Docker is not reachable.
    DockerUnavailable,
    /// Exit 1: the engine container failed to start or answer.
    EngineFailed,
    /// Exit 1: the engine answered a call with a JSON-RPC error.
    RpcError,
    /// Exit 1: `fund` read back a balance other than the one it set.
    ReadbackMismatch,
    /// Exit 1: anything else.
    Internal,
    /// Exit 2: bad arguments.
    Usage,
    /// Exit 3: this engine lacks the operation; `next` names the substitute.
    Unsupported,
    /// Exit 4: no fork with that name.
    ForkNotFound,
    /// Exit 4: no profile with that name.
    UnknownChain,
    /// Exit 4: no token with that symbol on the profile.
    UnknownToken,
    /// Exit 4: no snapshot with that id.
    SnapshotNotFound,
    /// Exit 5: the upstream RPC failed or timed out.
    UpstreamUnavailable,
    /// Exit 5: the upstream RPC rate-limited the engine.
    RateLimited,
    /// Exit 6: at least one selftest check failed; `data` holds every check.
    SelftestFailed,
    /// Exit 7: `fund` refused an address that is in use on mainnet.
    MainnetAddress,
    /// Exit 8: the name is taken by a fork of another chain or fork point.
    NameConflict,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 16] = [
        Self::DockerUnavailable,
        Self::EngineFailed,
        Self::RpcError,
        Self::ReadbackMismatch,
        Self::Internal,
        Self::Usage,
        Self::Unsupported,
        Self::ForkNotFound,
        Self::UnknownChain,
        Self::UnknownToken,
        Self::SnapshotNotFound,
        Self::UpstreamUnavailable,
        Self::RateLimited,
        Self::SelftestFailed,
        Self::MainnetAddress,
        Self::NameConflict,
    ];

    pub fn exit_code(self) -> i32 {
        match self {
            Self::DockerUnavailable | Self::EngineFailed | Self::RpcError | Self::ReadbackMismatch | Self::Internal => 1,
            Self::Usage => 2,
            Self::Unsupported => 3,
            Self::ForkNotFound | Self::UnknownChain | Self::UnknownToken | Self::SnapshotNotFound => 4,
            Self::UpstreamUnavailable | Self::RateLimited => 5,
            Self::SelftestFailed => 6,
            Self::MainnetAddress => 7,
            Self::NameConflict => 8,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DockerUnavailable => "docker_unavailable",
            Self::EngineFailed => "engine_failed",
            Self::RpcError => "rpc_error",
            Self::ReadbackMismatch => "readback_mismatch",
            Self::Internal => "internal",
            Self::Usage => "usage",
            Self::Unsupported => "unsupported",
            Self::ForkNotFound => "fork_not_found",
            Self::UnknownChain => "unknown_chain",
            Self::UnknownToken => "unknown_token",
            Self::SnapshotNotFound => "snapshot_not_found",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::RateLimited => "rate_limited",
            Self::SelftestFailed => "selftest_failed",
            Self::MainnetAddress => "mainnet_address",
            Self::NameConflict => "name_conflict",
        }
    }
}

/// A failed command: its code, a message, the command that fixes or works
/// around it, and optional detail (the selftest checks on exit 6).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub next: Option<String>,
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), next: None, data: None }
    }

    pub fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    pub fn data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn unsupported(operation: &str, substitute: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unsupported, format!("{operation} is not supported on this engine")).next(substitute)
    }

    pub fn fork_not_found(name: &str) -> Self {
        Self::new(ErrorCode::ForkNotFound, format!("no fork named {name}")).next("chainsim ls")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `error.next` for an upstream failure (exit 5). It names the variable that
/// takes a provider URL, never its value. Without the name (a caller that has
/// no profile) it points at `describe`.
pub fn upstream_next(env: Option<&str>, rate_limited: bool) -> String {
    let set = match env {
        Some(env) => format!("export {env}=<provider URL>"),
        None => "set the chain's upstream env variable (chainsim describe <chain>) to a provider URL".to_string(),
    };
    if rate_limited {
        format!("retry later, or {set} for a higher rate limit")
    } else {
        format!("retry, or {set}")
    }
}

/// `error.next` when a fork point is older than the upstream keeps state for.
pub fn archive_next(env: &str) -> String {
    format!("an old --block needs an archive upstream: export {env}=<archive provider URL>, or drop --block to fork at the head")
}
