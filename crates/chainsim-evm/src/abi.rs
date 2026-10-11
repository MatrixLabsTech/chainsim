//! Calldata for `send --sig` and revert reasons, with alloy-primitives only.
//! Covers the flat types an admin call takes: address, bool, uint<N>,
//! int<N>, bytes<N>, bytes and string. Arrays and tuples go through `--data`.

use alloy_primitives::{hex, keccak256, Address, I256, U256};
use chainsim_core::{Error, ErrorCode, Result};

fn usage(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::Usage, msg).next("pass the calldata with --data <hex> instead, for example from cast calldata")
}

/// Splits `name(type,type)` into the canonical signature and its types.
pub fn parse_signature(sig: &str) -> Result<(String, Vec<String>)> {
    let sig: String = sig.chars().filter(|c| !c.is_whitespace()).collect();
    let (name, rest) = sig.split_once('(').ok_or_else(|| usage(format!("--sig {sig:?} is not name(types)")))?;
    let inner = rest.strip_suffix(')').ok_or_else(|| usage(format!("--sig {sig:?} is not name(types)")))?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(usage(format!("--sig {sig:?} has no function name")));
    }
    if inner.contains(['(', ')', '[', ']']) {
        return Err(usage(format!("--sig {sig:?} has a tuple or array argument")));
    }
    let types: Vec<String> = if inner.is_empty() { vec![] } else { inner.split(',').map(canonical).collect::<Result<_>>()? };
    Ok((format!("{name}({})", types.join(",")), types))
}

/// `uint` and `int` are `uint256` and `int256` in a selector.
fn canonical(t: &str) -> Result<String> {
    let t = match t {
        "uint" => "uint256",
        "int" => "int256",
        t => t,
    };
    let bits = |rest: &str, max: u32, step: u32| rest.parse::<u32>().ok().filter(|n| *n >= step && *n <= max && n % step == 0);
    let ok = match t {
        "address" | "bool" | "string" | "bytes" => true,
        _ if t.starts_with("uint") => bits(&t[4..], 256, 8).is_some(),
        _ if t.starts_with("int") => bits(&t[3..], 256, 8).is_some(),
        _ if t.starts_with("bytes") => bits(&t[5..], 32, 1).is_some(),
        _ => false,
    };
    if ok {
        Ok(t.to_string())
    } else {
        Err(usage(format!("type {t} is not supported by --sig")))
    }
}

fn word_of(t: &str, arg: &str) -> Result<[u8; 32]> {
    let bad = || usage(format!("{arg:?} is not a valid {t}"));
    let mut w = [0u8; 32];
    match t {
        "address" => w[12..].copy_from_slice(arg.parse::<Address>().map_err(|_| bad())?.as_slice()),
        "bool" => w[31] = match arg {
            "true" => 1,
            "false" => 0,
            _ => return Err(bad()),
        },
        _ if t.starts_with("uint") => {
            let v: U256 = arg.parse().map_err(|_| bad())?;
            let bits: usize = t[4..].parse().unwrap();
            if bits < 256 && v >> bits != U256::ZERO {
                return Err(bad());
            }
            w = v.to_be_bytes();
        }
        _ if t.starts_with("int") => {
            let v: I256 = arg.parse().map_err(|_| bad())?;
            let bits: usize = t[3..].parse().unwrap();
            if bits < 256 {
                let bound = I256::ONE << (bits - 1);
                if v >= bound || v < -bound {
                    return Err(bad());
                }
            }
            w = v.to_be_bytes::<32>();
        }
        _ => {
            // bytes<N>: left-aligned, zero-padded on the right.
            let n: usize = t[5..].parse().unwrap();
            let b = hex::decode(arg).map_err(|_| bad())?;
            if b.len() > n {
                return Err(bad());
            }
            w[..b.len()].copy_from_slice(&b);
        }
    }
    Ok(w)
}

/// A dynamic value: its length word, then the bytes padded to 32.
fn tail_of(t: &str, arg: &str) -> Result<Vec<u8>> {
    let b = if t == "string" { arg.as_bytes().to_vec() } else { hex::decode(arg).map_err(|_| usage(format!("{arg:?} is not hex bytes")))? };
    let mut out = U256::from(b.len()).to_be_bytes::<32>().to_vec();
    out.extend_from_slice(&b);
    out.resize(32 + b.len().div_ceil(32) * 32, 0);
    Ok(out)
}

/// Calldata for `sig` with `args`: the 4-byte selector, then the ABI encoding.
pub fn encode_call(sig: &str, args: &[String]) -> Result<String> {
    let (canonical, types) = parse_signature(sig)?;
    if types.len() != args.len() {
        return Err(Error::new(ErrorCode::Usage, format!("{canonical} takes {} argument(s), got {}", types.len(), args.len())));
    }
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for (t, a) in types.iter().zip(args) {
        if t == "string" || t == "bytes" {
            head.extend_from_slice(&U256::from(32 * types.len() + tail.len()).to_be_bytes::<32>());
            tail.extend(tail_of(t, a)?);
        } else {
            head.extend_from_slice(&word_of(t, a)?);
        }
    }
    let mut out = keccak256(canonical.as_bytes())[..4].to_vec();
    out.extend(head);
    out.extend(tail);
    Ok(format!("0x{}", hex::encode(out)))
}

/// The reason in revert data: `Error(string)`, `Panic(uint256)`, or the
/// selector of a custom error.
pub fn revert_reason(data: &str) -> Option<String> {
    let b = hex::decode(data).ok()?;
    if b.len() < 4 {
        return None;
    }
    let (sel, body) = b.split_at(4);
    match sel {
        [0x08, 0xc3, 0x79, 0xa0] if body.len() >= 64 => {
            let len = U256::from_be_slice(&body[32..64]).to::<usize>();
            body.get(64..64 + len).map(|s| String::from_utf8_lossy(s).into_owned())
        }
        [0x4e, 0x48, 0x7b, 0x71] if body.len() >= 32 => Some(format!("panic 0x{:x}", U256::from_be_slice(&body[..32]))),
        _ => Some(format!("custom error 0x{}", hex::encode(sel))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn encodes_like_cast_calldata() {
        // Reference values from `cast calldata` (foundry v1.8.5).
        assert_eq!(encode_call("pause()", &[]).unwrap(), "0x8456cb59");
        assert_eq!(
            encode_call("transfer(address,uint256)", &s(&["0x000000000000000000000000000000000000c5e2", "1000000"])).unwrap(),
            "0xa9059cbb000000000000000000000000000000000000000000000000000000000000c5e200000000000000000000000000000000000000000000000000000000000f4240"
        );
        assert_eq!(encode_call("f(uint)", &s(&["0x10"])).unwrap(), encode_call("f(uint256)", &s(&["16"])).unwrap());
        assert_eq!(
            encode_call("f(int8,bool)", &s(&["-1", "true"])).unwrap(),
            "0x32c65fd2ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff0000000000000000000000000000000000000000000000000000000000000001"
        );
        assert_eq!(
            encode_call("f(string,uint256)", &s(&["hi", "1"])).unwrap(),
            "0x589002fb0000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000026869000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            encode_call("f(bytes,bytes4,string)", &s(&["0x0102", "0xdeadbeef", ""])).unwrap(),
            "0x8506b6e90000000000000000000000000000000000000000000000000000000000000060deadbeef0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000201020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(encode_call("f(int256)", &s(&["-5"])).unwrap(), "0x1c008df9fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffb");
    }

    #[test]
    fn refuses_what_it_cannot_encode() {
        for (sig, args) in [("f(uint256[])", vec!["1"]), ("f((uint256,bool))", vec!["1"]), ("f(uint8)", vec!["256"]), ("f(address)", vec!["0x12"]), ("f(uint256)", vec![])] {
            let e = encode_call(sig, &s(&args)).unwrap_err();
            assert_eq!(e.code, ErrorCode::Usage, "{sig}");
        }
    }

    #[test]
    fn decodes_revert_reasons() {
        let msg = encode_call("Error(string)", &s(&["Pausable: caller is not the pauser"])).unwrap();
        assert_eq!(revert_reason(&msg).as_deref(), Some("Pausable: caller is not the pauser"));
        let panic = encode_call("Panic(uint256)", &s(&["17"])).unwrap();
        assert_eq!(revert_reason(&panic).as_deref(), Some("panic 0x11"));
        assert_eq!(revert_reason("0xdeadbeef").as_deref(), Some("custom error 0xdeadbeef"));
        assert_eq!(revert_reason("0x"), None);
    }
}
