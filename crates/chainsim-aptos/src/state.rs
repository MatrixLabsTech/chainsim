//! A simulation session's `delta.json`: every state change of the copy, keyed
//! by the session's human-readable StateKey (`resource::<address>::<struct tag>`)
//! with the hex of the BCS StateValue (aptos-core aptos-cli-v9.6.0,
//! aptos-move/aptos-transaction-simulation-session/src/state_store.rs and delta.rs).
//! chainsim writes it for impersonation (an Account resource whose
//! authentication key is the fork's control key) and for `set resource`.

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::Value;
use sha3::{Digest, Sha3_256};

/// An Aptos address as 0x plus 64 lowercase hex digits.
pub fn parse_address(s: &str) -> Result<String> {
    let hex = s.trim().strip_prefix("0x").unwrap_or("");
    if hex.is_empty() || hex.len() > 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorCode::Usage, format!("{s} is not an Aptos address (0x and up to 64 hex digits)")));
    }
    Ok(format!("0x{:0>64}", hex.to_lowercase()))
}

/// The AIP-40 form the session writes: the special addresses 0x0 to 0xf
/// short, every other address as 64 hex digits.
pub fn standard_address(s: &str) -> Result<String> {
    let long = parse_address(s)?;
    let digits = &long[2..];
    if digits[..63].bytes().all(|b| b == b'0') {
        return Ok(format!("0x{}", &digits[63..]));
    }
    Ok(long)
}

/// The 32 bytes of an address.
pub fn address_bytes(s: &str) -> Result<[u8; 32]> {
    let long = parse_address(s)?;
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&long[2 + 2 * i..4 + 2 * i], 16).expect("parse_address checked the digits");
    }
    Ok(out)
}

/// The single-key Ed25519 authentication key of a public key, which is also
/// the address of an account created for it: sha3-256(public key || 0x00).
pub fn ed25519_auth_key(public_key: &[u8]) -> [u8; 32] {
    let mut h = Sha3_256::new();
    h.update(public_key);
    h.update([0u8]);
    h.finalize().into()
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex with or without 0x to bytes.
pub fn from_hex(s: &str, what: &str) -> Result<Vec<u8>> {
    let hex = s.trim().strip_prefix("0x").unwrap_or(s.trim());
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorCode::Usage, format!("{what} is not hex")));
    }
    Ok((0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked")).collect())
}

/// A struct tag in the canonical form the session writes: every address in
/// AIP-40 form, no spaces except one after each comma between type arguments.
pub fn canonical_struct_tag(tag: &str) -> Result<String> {
    let bad = || Error::new(ErrorCode::Usage, format!("{tag} is not a Move struct tag such as 0x1::account::Account"));
    let compact: String = tag.chars().filter(|c| !c.is_whitespace()).collect();
    let base = compact.split('<').next().unwrap_or("");
    let parts: Vec<&str> = base.split("::").collect();
    let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.starts_with(|c: char| c.is_ascii_digit());
    if parts.len() != 3 || !parts[0].starts_with("0x") || !ident(parts[1]) || !ident(parts[2]) {
        return Err(bad());
    }
    if compact.contains('<') != compact.ends_with('>') || compact.matches('<').count() != compact.matches('>').count() {
        return Err(bad());
    }
    // Rewrite each address token, then put one space after each comma.
    let mut out = String::with_capacity(compact.len());
    let mut rest = compact.as_str();
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i]);
        // An address starts the tag or follows `<` or `,`; a 0x inside an identifier stays.
        let starts_token = matches!(out.chars().last(), None | Some('<' | ','));
        let tail = &rest[i..];
        let end = 2 + tail[2..].find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(tail.len() - 2);
        if starts_token {
            out.push_str(&standard_address(&tail[..end]).map_err(|_| bad())?);
        } else {
            out.push_str(&tail[..end]);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    Ok(out.replace(',', ", "))
}

/// The delta key of a resource under an account.
pub fn resource_key(address: &str, struct_tag: &str) -> Result<String> {
    Ok(format!("resource::{}::{}", standard_address(address)?, canonical_struct_tag(struct_tag)?))
}

pub fn uleb128(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

/// The delta value of a resource: the BCS StateValue variant 0 (bytes with no
/// storage metadata) as hex.
pub fn state_value_hex(bytes: &[u8]) -> String {
    let mut v = vec![0u8];
    v.extend(uleb128(bytes.len()));
    v.extend_from_slice(bytes);
    to_hex(&v)
}

fn u64_of(v: &Value, field: &str) -> Result<u64> {
    let bad = || Error::new(ErrorCode::RpcError, format!("the Account resource has no u64 {field}"));
    match v {
        Value::String(s) => s.parse().map_err(|_| bad()),
        Value::Number(n) => n.as_u64().ok_or_else(bad),
        _ => Err(bad()),
    }
}

fn event_handle(out: &mut Vec<u8>, v: &Value, field: &str) -> Result<()> {
    out.extend(u64_of(&v["counter"], field)?.to_le_bytes());
    out.extend(u64_of(&v["guid"]["id"]["creation_num"], field)?.to_le_bytes());
    out.extend(address_bytes(v["guid"]["id"]["addr"].as_str().unwrap_or(""))?);
    Ok(())
}

fn capability_offer(out: &mut Vec<u8>, v: &Value) -> Result<()> {
    let items = v["for"]["vec"].as_array().cloned().unwrap_or_default();
    out.extend(uleb128(items.len()));
    for a in items {
        out.extend(address_bytes(a.as_str().unwrap_or(""))?);
    }
    Ok(())
}

/// The BCS of `0x1::account::Account` from its JSON (as `sim view-resource`
/// prints it), with the authentication key replaced by `auth_key`.
pub fn account_bytes(account: &Value, auth_key: &[u8; 32]) -> Result<Vec<u8>> {
    let mut out = uleb128(32);
    out.extend_from_slice(auth_key);
    out.extend(u64_of(&account["sequence_number"], "sequence_number")?.to_le_bytes());
    out.extend(u64_of(&account["guid_creation_num"], "guid_creation_num")?.to_le_bytes());
    event_handle(&mut out, &account["coin_register_events"], "coin_register_events")?;
    event_handle(&mut out, &account["key_rotation_events"], "key_rotation_events")?;
    capability_offer(&mut out, &account["rotation_capability_offer"])?;
    capability_offer(&mut out, &account["signer_capability_offer"])?;
    Ok(out)
}

/// The Account resource `0x1::account::create_account` makes for an address
/// with none, with `auth_key` as its authentication key: sequence number 0 and
/// its two event handles.
pub fn new_account_bytes(address: &str, auth_key: &[u8; 32]) -> Result<Vec<u8>> {
    let addr = parse_address(address)?;
    let handle = |n: u64| serde_json::json!({ "counter": "0", "guid": { "id": { "creation_num": n.to_string(), "addr": addr } } });
    let account = serde_json::json!({
        "sequence_number": "0",
        "guid_creation_num": "2",
        "coin_register_events": handle(0),
        "key_rotation_events": handle(1),
        "rotation_capability_offer": { "for": { "vec": [] } },
        "signer_capability_offer": { "for": { "vec": [] } },
    });
    account_bytes(&account, auth_key)
}

/// The patch file `merge_script` reads: one `key<TAB>"hex"` line per entry.
pub fn patch_lines(entries: &[(String, String)]) -> String {
    entries.iter().map(|(k, v)| format!("{k}\t\"{v}\"\n")).collect()
}

/// Merges a patch file into `delta.json`: entries in the patch replace those
/// with the same key, the rest stay. The session writes `delta.json` with
/// serde_json's pretty printer, one `  "key": value` line per entry, and keys
/// hold no quotes, so a line is split at its first `": `.
pub const MERGE_AWK: &str = r#"NR == FNR { if ($0 == "") next; t = index($0, "\t"); k = substr($0, 1, t - 1); NEW[k] = substr($0, t + 1); ORD[++m] = k; next }
/^  "/ { line = $0; sub(/,$/, "", line); s = index(line, "\": "); k = substr(line, 4, s - 4); if (!(k in NEW)) { OK[++o] = k; OV[o] = substr(line, s + 3) } }
END { print "{"; c = 0; for (i = 1; i <= o; i++) printf "%s  \"%s\": %s", (c++ ? ",\n" : ""), OK[i], OV[i]; for (i = 1; i <= m; i++) printf "%s  \"%s\": %s", (c++ ? ",\n" : ""), ORD[i], NEW[ORD[i]]; if (c) printf "\n"; print "}" }"#;

/// The shell script that writes the patch (its first argument) and merges it
/// into the session's delta under the fork lock.
pub fn merge_script(session: &str, lock: &str, patch: &str) -> String {
    format!(
        "set -e; exec 9>{lock}; flock 9; printf '%s' \"$1\" > {patch}; \
         awk '{}' {patch} {session}/delta.json > {session}/delta.tmp; mv {session}/delta.tmp {session}/delta.json; rm -f {patch}",
        MERGE_AWK.replace('\'', r"'\''")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ADDR: &str = "0x1e3532a073510cead7a7532083c73b31fcca8ab4bcdb41c7ea947828dcc2e8dc";
    /// The Account bytes the CLI itself wrote to delta.json for ADDR after two
    /// transactions (aptos 9.6.0, measured 2026-11-01), without its metadata.
    const ORACLE: &str = "201e3532a073510cead7a7532083c73b31fcca8ab4bcdb41c7ea947828dcc2e8dc02000000000000000200000000000000000000000000000000000000000000001e3532a073510cead7a7532083c73b31fcca8ab4bcdb41c7ea947828dcc2e8dc000000000000000001000000000000001e3532a073510cead7a7532083c73b31fcca8ab4bcdb41c7ea947828dcc2e8dc0000";

    fn handle(n: &str) -> Value {
        json!({ "counter": "0", "guid": { "id": { "addr": ADDR, "creation_num": n } } })
    }

    #[test]
    fn addresses_and_auth_keys() {
        assert_eq!(parse_address("0xA").unwrap(), format!("0x{}a", "0".repeat(63)));
        assert!(parse_address("a").is_err() && parse_address("0x").is_err() && parse_address(&format!("0x{}", "1".repeat(65))).is_err());
        assert_eq!(standard_address("0x0000a").unwrap(), "0xa");
        assert_eq!(standard_address("0x10").unwrap(), format!("0x{}10", "0".repeat(62)));
        assert_eq!(standard_address(ADDR).unwrap(), ADDR);
        // A key the CLI generated (aptos key generate) and the address the CLI used for it.
        let public = from_hex("02160BC5D99727A065A02AF694CF5EB0D589CCF9EB45631183293DAA317DE780", "key").unwrap();
        assert_eq!(format!("0x{}", to_hex(&ed25519_auth_key(&public))), ADDR);
    }

    #[test]
    fn state_keys_match_the_session_format() {
        assert_eq!(resource_key(ADDR, "0x1::account::Account").unwrap(), format!("resource::{ADDR}::0x1::account::Account"));
        assert_eq!(resource_key("0x00001", "0x0001::coin::CoinStore< 0x1::aptos_coin::AptosCoin >").unwrap(), "resource::0x1::0x1::coin::CoinStore<0x1::aptos_coin::AptosCoin>");
        assert_eq!(canonical_struct_tag("0x1::m::T<u64,0x1::m::U<bool,u8>>").unwrap(), "0x1::m::T<u64, 0x1::m::U<bool, u8>>");
        let usdc = "0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b";
        assert_eq!(canonical_struct_tag(&format!("0x1::m::T<{usdc}::x::Y>")).unwrap(), format!("0x1::m::T<{usdc}::x::Y>"));
        for bad in ["account::Account", "0x1::account", "0x1::a::B<", "0x1::a-b::C", "0x1::a::B>"] {
            assert!(canonical_struct_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_values_are_variant_0_with_a_uleb_length() {
        assert_eq!(uleb128(0), [0]);
        assert_eq!(uleb128(127), [0x7f]);
        assert_eq!(uleb128(147), [0x93, 0x01]);
        assert_eq!(state_value_hex(&[0xab]), "0001ab");
        let bytes = from_hex(ORACLE, "oracle").unwrap();
        assert_eq!(&state_value_hex(&bytes)[..6], "009301");
    }

    #[test]
    fn the_account_encoder_matches_the_cli_bytes() {
        let account = json!({
            "authentication_key": ADDR,
            "coin_register_events": handle("0"),
            "guid_creation_num": "2",
            "key_rotation_events": handle("1"),
            "rotation_capability_offer": { "for": { "vec": [] } },
            "sequence_number": "2",
            "signer_capability_offer": { "for": { "vec": [] } },
        });
        let key = address_bytes(ADDR).unwrap();
        assert_eq!(to_hex(&account_bytes(&account, &key).unwrap()), ORACLE);
        // The auth-key patch replaces only the 32 key bytes.
        let other = [7u8; 32];
        let patched = account_bytes(&account, &other).unwrap();
        assert_eq!(&patched[1..33], &other);
        assert_eq!(&patched[33..], &from_hex(ORACLE, "oracle").unwrap()[33..]);
        // An offer that names an address is a one-element option.
        let mut offered = account.clone();
        offered["signer_capability_offer"] = json!({ "for": { "vec": [ADDR] } });
        let b = account_bytes(&offered, &key).unwrap();
        assert_eq!(b.len(), 147 + 32);
        assert_eq!(&b[b.len() - 33..], &[&[1u8][..], &address_bytes(ADDR).unwrap()[..]].concat()[..]);
    }

    #[test]
    fn a_new_account_has_sequence_0_and_its_two_handles() {
        let key = [9u8; 32];
        let b = new_account_bytes(ADDR, &key).unwrap();
        let mut want = vec![32u8];
        want.extend(key);
        want.extend(0u64.to_le_bytes());
        want.extend(2u64.to_le_bytes());
        for n in [0u64, 1] {
            want.extend(0u64.to_le_bytes());
            want.extend(n.to_le_bytes());
            want.extend(address_bytes(ADDR).unwrap());
        }
        want.extend([0, 0]);
        assert_eq!(b, want);
    }

    #[test]
    fn patch_lines_and_the_merge_script() {
        let lines = patch_lines(&[("resource::0x1::0x1::m::T".into(), "00ab".into())]);
        assert_eq!(lines, "resource::0x1::0x1::m::T\t\"00ab\"\n");
        let script = merge_script("/data/session", "/data/lock", "/data/patch");
        assert!(script.contains("flock 9") && script.contains("awk 'NR == FNR") && script.contains("/data/session/delta.json"));
    }

    /// Runs the merge script for real where sh, awk and flock exist (the Docker
    /// test image has them), on a delta in the session's own format.
    #[test]
    fn the_merge_script_replaces_and_adds_entries() {
        let have = |tool: &str| std::process::Command::new("sh").args(["-c", &format!("command -v {tool}")]).output().is_ok_and(|o| o.status.success());
        if !(have("awk") && have("flock")) {
            eprintln!("skipped: awk or flock missing");
            return;
        }
        let dir = std::env::temp_dir().join(format!("chainsim-aptos-merge-{}", std::process::id()));
        let session = dir.join("session");
        std::fs::create_dir_all(&session).unwrap();
        let run = |patch: &str| {
            let script = merge_script(session.to_str().unwrap(), dir.join("lock").to_str().unwrap(), dir.join("patch").to_str().unwrap());
            let out = std::process::Command::new("sh").args(["-c", &script, "sh", patch]).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let text = std::fs::read_to_string(session.join("delta.json")).unwrap();
            serde_json::from_str::<serde_json::Map<String, Value>>(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
        };
        // An empty delta, as `sim init` writes it.
        std::fs::write(session.join("delta.json"), "{}").unwrap();
        let first = run(&patch_lines(&[("resource::0x1::0x1::m::T<u64, bool>".into(), "0001aa".into())]));
        assert_eq!(first.len(), 1);
        assert_eq!(first["resource::0x1::0x1::m::T<u64, bool>"], "0001aa");
        // A delta the session wrote, with a null (deleted) entry.
        std::fs::write(session.join("delta.json"), "{\n  \"resource::0x2::0x1::m::A\": \"0001bb\",\n  \"resource::0x3::0x1::m::B\": null,\n  \"resource::0x1::0x1::m::T<u64, bool>\": \"0001aa\"\n}").unwrap();
        let merged = run(&patch_lines(&[("resource::0x1::0x1::m::T<u64, bool>".into(), "0001cc".into()), ("resource::0x4::0x1::m::C".into(), "0001dd".into())]));
        assert_eq!(merged.len(), 4);
        assert_eq!(merged["resource::0x2::0x1::m::A"], "0001bb");
        assert!(merged["resource::0x3::0x1::m::B"].is_null());
        assert_eq!(merged["resource::0x1::0x1::m::T<u64, bool>"], "0001cc");
        assert_eq!(merged["resource::0x4::0x1::m::C"], "0001dd");
        assert!(!dir.join("patch").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
