//! The fork's modification log: every `send --as` and `set`, so a report can
//! say which results depend on impersonation or a state override. It lives in
//! the control sidecar's tmpfs, one JSON object per line, so it goes with the
//! fork at `down` and needs no state file on the host.

use serde_json::Value;

use crate::docker::{self, Docker};
use crate::error::{Error, ErrorCode, Result};

const LOG: &str = "/tmp/chainsim-modifications.jsonl";

/// What `send` and `set` add to their output.
pub const NOTE: &str = "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down.";

/// Appends one modification to the fork's log.
pub async fn record(docker: &Docker, fork: &str, entry: &Value) -> Result<()> {
    let line = entry.to_string();
    let argv = ["sh", "-c", &format!("printf '%s\\n' \"$1\" >> {LOG}"), "sh", &line].map(String::from);
    let out = docker.exec(&docker::control_container(fork), &argv).await?;
    if out.exit_code != 0 {
        return Err(Error::new(ErrorCode::Internal, format!("could not record the modification: {}", out.stderr.trim())));
    }
    Ok(())
}

/// Every modification since `up`, oldest first. A fork without any has none.
pub async fn list(docker: &Docker, fork: &str) -> Result<Vec<Value>> {
    let argv = ["sh", "-c", &format!("cat {LOG} 2>/dev/null || true")].map(String::from);
    let out = docker.exec(&docker::control_container(fork), &argv).await?;
    Ok(parse(&out.stdout))
}

/// The log's lines as JSON values; a line that is not JSON is skipped.
pub fn parse(log: &str) -> Vec<Value> {
    log.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_log_round_trips_one_object_per_line() {
        let a = json!({ "kind": "send-as", "from": "0xab", "note": "it's \"quoted\"\nand multi-line" });
        let b = json!({ "kind": "set-storage", "slot": "0x0" });
        let log = format!("{a}\n{b}\nnot json\n");
        assert_eq!(parse(&log), [a, b]);
        assert!(parse("").is_empty());
    }
}
