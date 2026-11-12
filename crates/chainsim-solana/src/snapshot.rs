//! `snapshot` and `revert`. The engine has no revert in place: a snapshot is
//! its account export kept on the fork volume, and `revert` restarts surfpool
//! inside the same container with `--snapshot`, then moves the clock forward
//! to the snapshot's.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chainsim_core::docker;
use chainsim_core::fork::{Reverted, Snapshot};
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::json;

use crate::time::clock;
use crate::SolanaAdapter;

/// Where the fork volume is mounted in the engine.
pub const DATA: &str = "/data";
/// How long a revert waits for surfpool to serve again.
const RESTART_TIMEOUT: Duration = Duration::from_secs(60);

/// The fork's data volume, labeled with the fork so `down` removes it.
pub fn data_volume(name: &str) -> String {
    format!("chainsim-{name}-data")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The engine container's command: surfpool with `args`, started again with
/// `--snapshot <file>` when a revert left a file name in `restore` and stopped
/// it. Any other exit ends the container.
pub fn engine_script(args: &[String]) -> String {
    let args = args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    format!(
        "D={DATA}; mkdir -p $D/snapshots; while :; do set -- {args}; [ -s $D/restore ] && set -- \"$@\" --snapshot \"$(cat $D/restore)\"; rm -f $D/restore; \
         surfpool \"$@\" & echo $! > /tmp/engine.pid; wait $!; [ -e $D/restore ] || exit 1; done"
    )
}

fn snapshot_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("s{nanos:x}")
}

fn valid_snapshot_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('s') && id[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Writes the engine's export, programs included, as `snapshots/<id>.json`
/// with the Clock beside it. The export's default leaves program accounts
/// out, and a program deployed on the fork would then be gone after a revert.
/// The JSON-RPC envelope is cut off, since `--snapshot` reads the bare map.
/// Exits 5 when the engine did not answer with an export.
fn export_script(id: &str, port: u16, slot: u64, time: u64) -> String {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"surfnet_exportSnapshot","params":[{"filter":{"includeProgramAccounts":true},"scope":"network"}]}"#;
    format!(
        "f={DATA}/snapshots/{id}; curl -s --fail -m 120 -H 'Content-Type: application/json' --data '{body}' http://127.0.0.1:{port} > $f.raw || exit 5; \
         head -c 40 $f.raw | grep -q '^{{\"jsonrpc\":\"2.0\",\"result\":{{\"context\":' || {{ rm -f $f.raw; exit 5; }}; \
         sed -e 's/^{{\"jsonrpc\":\"2.0\",\"result\":{{\"context\":{{[^}}]*}},\"value\"://' -e 's/}},\"id\":1}}$//' $f.raw > $f.json && rm -f $f.raw && \
         [ \"$(head -c 2 $f.json)\" = '{{\"' ] && printf '%s %s\\n' {slot} {time} > $f.clock || {{ rm -f $f.raw $f.json; exit 5; }}"
    )
}

/// Keeps snapshot `old` as the fresh snapshot `new`, names it for the next
/// start, and stops surfpool so the engine script starts it from that file.
/// Prints the snapshot's Clock. Exits 4 when `old` does not exist.
fn revert_script(old: &str, new: &str) -> String {
    format!(
        "S={DATA}/snapshots; [ -s $S/{old}.json ] && [ -s $S/{old}.clock ] || exit 4; set -e; cat $S/{old}.clock; \
         mv $S/{old}.json $S/{new}.json; mv $S/{old}.clock $S/{new}.clock; echo $S/{new}.json > {DATA}/restore; \
         p=$(cat /tmp/engine.pid); kill $p; i=0; while kill -0 $p 2>/dev/null; do i=$((i+1)); [ $i -gt 100 ] && kill -9 $p; sleep 0.1; done"
    )
}

pub async fn snapshot(adapter: &SolanaAdapter, name: &str) -> Result<Snapshot> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let (slot, time) = clock(&rpc).await?;
    let id = snapshot_id();
    let script = export_script(&id, profile.rpc_port(), slot, time);
    let out = adapter.docker.exec(&docker::engine_container(name), &["sh".into(), "-c".into(), script]).await?;
    match out.exit_code {
        0 => Ok(Snapshot { id }),
        c => Err(Error::new(ErrorCode::EngineFailed, format!("snapshot failed: the export step exited {c}: {}", rpc.control.scrub(out.stderr.trim())))),
    }
}

pub async fn revert(adapter: &SolanaAdapter, name: &str, id: &str) -> Result<Reverted> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let missing = || Error::new(ErrorCode::SnapshotNotFound, format!("no snapshot {id} on {name}: it was never taken or was already used")).next(format!("chainsim snapshot {name}"));
    if !valid_snapshot_id(id) {
        return Err(missing());
    }
    let fresh = snapshot_id();
    let engine = docker::engine_container(name);
    let out = adapter.docker.exec(&engine, &["sh".into(), "-c".into(), revert_script(id, &fresh)]).await?;
    let failed = |what: String| Error::new(ErrorCode::EngineFailed, format!("revert failed: {what}")).next(format!("chainsim down {name} && chainsim up {} --name {name}", profile.key));
    match out.exit_code {
        0 => {}
        4 => return Err(missing()),
        c => return Err(failed(format!("the restart step exited {c}: {}", rpc.control.scrub(out.stderr.trim())))),
    }
    let mut stored = out.stdout.split_whitespace().map(|n| n.parse::<u64>().ok());
    let (Some(Some(slot)), Some(Some(time))) = (stored.next(), stored.next()) else {
        return Err(failed(format!("the snapshot's clock reads {:?}", out.stdout.trim())));
    };

    let started = Instant::now();
    loop {
        if !adapter.docker.is_running(&engine).await {
            return Err(failed("the engine stopped while starting from the snapshot".into()));
        }
        if rpc.call_timeout("getHealth", json!([]), Duration::from_secs(5)).await.is_ok_and(|v| v == "ok") {
            break;
        }
        if started.elapsed() > RESTART_TIMEOUT {
            return Err(failed(format!("the engine did not answer within {} s", RESTART_TIMEOUT.as_secs())));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // The engine comes back at the upstream's current slot and time. A
    // snapshot taken after a time jump is ahead of that: move forward to it.
    if clock(&rpc).await?.1 < time {
        rpc.call("surfnet_timeTravel", json!([{ "absoluteTimestamp": time * 1000 }])).await?;
    }
    if rpc.slot().await? < slot {
        rpc.call("surfnet_timeTravel", json!([{ "absoluteSlot": slot }])).await?;
    }
    adapter.record(name, json!({ "kind": "revert", "snapshot": id, "note": "account changes listed above this entry and after that snapshot are undone" })).await?;
    Ok(Reverted { reverted: id.to_string(), snapshot: fresh })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_ids_are_safe_in_scripts() {
        let id = snapshot_id();
        assert!(valid_snapshot_id(&id), "{id}");
        for bad in ["s1; rm -rf /", "../x", "s", "", "s1 2"] {
            assert!(!valid_snapshot_id(bad), "{bad}");
        }
        assert!(revert_script("s1", "s2").starts_with("S=/data/snapshots; [ -s $S/s1.json ] && [ -s $S/s1.clock ] || exit 4;"));
        let export = export_script("s1", 8899, 7, 9);
        assert!(export.contains(r#""params":[{"filter":{"includeProgramAccounts":true},"scope":"network"}]"#), "{export}");
        assert!(export.contains(r#"sed -e 's/^{"jsonrpc":"2.0","result":{"context":{[^}]*},"value"://' -e 's/},"id":1}$//' $f.raw > $f.json"#), "{export}");
        assert!(export.contains("printf '%s %s\\n' 7 9 > $f.clock"), "{export}");
    }

    #[test]
    fn the_engine_script_restarts_only_for_a_revert() {
        let script = engine_script(&["start".to_string(), "--port".to_string(), "8899".to_string(), "it's".to_string()]);
        assert!(script.contains("set -- 'start' '--port' '8899' 'it'\\''s';"), "{script}");
        assert!(script.contains("[ -s $D/restore ] && set -- \"$@\" --snapshot \"$(cat $D/restore)\"; rm -f $D/restore;"), "{script}");
        assert!(script.ends_with("wait $!; [ -e $D/restore ] || exit 1; done"), "{script}");
        assert!(!script.contains("://"));
    }
}
