//! Programmable transactions for `send --as`: the Move call target, pure
//! argument encoding from a function's parameter types, and the summary of
//! a transaction's effects. Everything here is pure; the adapter resolves
//! objects and runs the transaction.

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::parse_address;
use crate::units::base64;

/// A Move call target: `package::module::function<T1, T2>`.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub package: String,
    pub module: String,
    pub function: String,
    pub type_args: Vec<String>,
}

pub fn parse_target(s: &str) -> Result<Target> {
    let usage = || {
        Error::new(ErrorCode::Usage, format!("{s} is not a Move function; use package::module::function, with type arguments in <...> if it has any"))
            .next("chainsim send <name> --as <address> --to '0x2::coin::mint_and_transfer<0x...::coin::COIN>' <arg>...")
    };
    let s = s.trim();
    let (path, type_args) = match s.find('<') {
        Some(i) => {
            let inner = s[i..].strip_prefix('<').and_then(|r| r.strip_suffix('>')).ok_or_else(usage)?;
            (&s[..i], split_top_level(inner).into_iter().map(|t| t.trim().to_string()).collect())
        }
        None => (s, Vec::new()),
    };
    let parts: Vec<&str> = path.split("::").collect();
    let [package, module, function] = parts[..] else { return Err(usage()) };
    if module.is_empty() || function.is_empty() || type_args.iter().any(String::is_empty) {
        return Err(usage());
    }
    Ok(Target { package: parse_address(package)?, module: module.into(), function: function.into(), type_args })
}

/// Splits `a, b<c, d>, e` at the commas outside angle brackets.
fn split_top_level(s: &str) -> Vec<&str> {
    let (mut depth, mut start, mut out) = (0i32, 0, Vec::new());
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// How one parameter takes its argument.
#[derive(Debug, PartialEq)]
pub enum Param {
    /// A BCS value; the bytes are already encoded.
    Pure(Vec<u8>),
    /// An object by ID; `mutable` comes from the parameter's reference.
    Object { id: String, mutable: bool },
    /// A `Receiving<T>` argument.
    Receiving { id: String },
}

/// True for the `&mut TxContext` the runtime passes itself.
pub fn is_tx_context(p: &Value) -> bool {
    p["body"]["typeName"].as_str().is_some_and(|t| t.ends_with("::tx_context::TxContext"))
}

/// Maps one argument string to a parameter, from the function's
/// `OpenSignature` and the call's type arguments.
pub fn param(sig: &Value, arg: &str, type_args: &[String]) -> Result<Param> {
    let mutable = sig["reference"] != json!("IMMUTABLE");
    match body_kind(&sig["body"], type_args)? {
        Kind::Pure(ty) => Ok(Param::Pure(encode_pure(&ty, arg)?)),
        Kind::Receiving => Ok(Param::Receiving { id: parse_address(arg)? }),
        Kind::Object => Ok(Param::Object { id: parse_address(arg)?, mutable }),
    }
}

enum Kind {
    Pure(PureType),
    Object,
    Receiving,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PureType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    String,
    Vector(Box<PureType>),
    Option(Box<PureType>),
}

fn body_kind(body: &Value, type_args: &[String]) -> Result<Kind> {
    match pure_of_body(body, type_args)? {
        Some(t) => Ok(Kind::Pure(t)),
        None => {
            let name = body_type_name(body, type_args);
            Ok(if name.ends_with("::transfer::Receiving") { Kind::Receiving } else { Kind::Object })
        }
    }
}

/// The datatype name of a body, after substituting a type parameter.
fn body_type_name(body: &Value, type_args: &[String]) -> String {
    if body["type"] == json!("TYPE_PARAMETER") {
        let i = body["typeParameter"].as_u64().unwrap_or(0) as usize;
        let t = type_args.get(i).map(String::as_str).unwrap_or("");
        return t.split('<').next().unwrap_or(t).to_string();
    }
    body["typeName"].as_str().unwrap_or("").to_string()
}

/// The pure type of a signature body, or None for an object.
fn pure_of_body(body: &Value, type_args: &[String]) -> Result<Option<PureType>> {
    let simple = match body["type"].as_str().unwrap_or("") {
        "BOOL" => PureType::Bool,
        "U8" => PureType::U8,
        "U16" => PureType::U16,
        "U32" => PureType::U32,
        "U64" => PureType::U64,
        "U128" => PureType::U128,
        "U256" => PureType::U256,
        "ADDRESS" => PureType::Address,
        "VECTOR" => {
            let inner = body["typeParameterInstantiation"].get(0).ok_or_else(|| Error::new(ErrorCode::Internal, "a vector parameter has no element type"))?;
            return Ok(pure_of_body(inner, type_args)?.map(|t| PureType::Vector(Box::new(t))));
        }
        "TYPE_PARAMETER" => {
            let i = body["typeParameter"].as_u64().unwrap_or(0) as usize;
            let t = type_args.get(i).ok_or_else(|| Error::new(ErrorCode::Usage, format!("the function needs type argument {i}; add it in <...> after the function")))?;
            return Ok(pure_of_type_tag(t));
        }
        "DATATYPE" => {
            let name = body["typeName"].as_str().unwrap_or("");
            if is_string_type(name) {
                PureType::String
            } else if name.ends_with("::object::ID") && name.starts_with(&format!("0x{:0>64}", "2")) {
                PureType::Address
            } else if name == format!("0x{:0>64}::option::Option", "1") {
                let inner = body["typeParameterInstantiation"].get(0).ok_or_else(|| Error::new(ErrorCode::Internal, "an Option parameter has no element type"))?;
                return Ok(pure_of_body(inner, type_args)?.map(|t| PureType::Option(Box::new(t))));
            } else {
                return Ok(None);
            }
        }
        other => return Err(Error::new(ErrorCode::Internal, format!("unknown parameter type {other:?}"))),
    };
    Ok(Some(simple))
}

fn is_string_type(name: &str) -> bool {
    let std = format!("0x{:0>64}", "1");
    name == format!("{std}::string::String") || name == format!("{std}::ascii::String")
}

/// The pure type of a type argument such as `u64` or `vector<u8>`, or None for a struct.
fn pure_of_type_tag(t: &str) -> Option<PureType> {
    let t = t.trim();
    Some(match t {
        "bool" => PureType::Bool,
        "u8" => PureType::U8,
        "u16" => PureType::U16,
        "u32" => PureType::U32,
        "u64" => PureType::U64,
        "u128" => PureType::U128,
        "u256" => PureType::U256,
        "address" => PureType::Address,
        _ => {
            let inner = t.strip_prefix("vector<")?.strip_suffix('>')?;
            PureType::Vector(Box::new(pure_of_type_tag(inner)?))
        }
    })
}

/// BCS for one argument string of a pure type.
pub fn encode_pure(ty: &PureType, arg: &str) -> Result<Vec<u8>> {
    let bad = |what: &str| Error::new(ErrorCode::Usage, format!("{arg:?} is not {what}"));
    Ok(match ty {
        PureType::Bool => match arg {
            "true" => vec![1],
            "false" => vec![0],
            _ => return Err(bad("true or false")),
        },
        PureType::U8 => uint(arg, 1).ok_or_else(|| bad("a u8"))?,
        PureType::U16 => uint(arg, 2).ok_or_else(|| bad("a u16"))?,
        PureType::U32 => uint(arg, 4).ok_or_else(|| bad("a u32"))?,
        PureType::U64 => uint(arg, 8).ok_or_else(|| bad("a u64"))?,
        PureType::U128 => uint(arg, 16).ok_or_else(|| bad("a u128"))?,
        PureType::U256 => uint(arg, 32).ok_or_else(|| bad("a u256"))?,
        PureType::Address => crate::units::address_bytes(&parse_address(arg)?).to_vec(),
        PureType::String => with_len(arg.as_bytes()),
        // vector<u8> takes 0x hex bytes, or else the text's UTF-8 bytes.
        PureType::Vector(inner) if **inner == PureType::U8 => match arg.strip_prefix("0x") {
            Some(h) => with_len(&hex(h).ok_or_else(|| bad("0x hex bytes"))?),
            None => with_len(arg.as_bytes()),
        },
        // Other vectors take a JSON array, such as [1,2] or ["0x1","0x2"].
        PureType::Vector(inner) => {
            let items: Vec<Value> = serde_json::from_str(arg).map_err(|_| bad("a JSON array"))?;
            let mut out = uleb128(items.len());
            for v in items {
                let s = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                out.extend(encode_pure(inner, &s)?);
            }
            out
        }
        // Option takes "none", or else the inner value.
        PureType::Option(inner) => match arg {
            "none" => vec![0],
            _ => [vec![1], encode_pure(inner, arg)?].concat(),
        },
    })
}

fn with_len(bytes: &[u8]) -> Vec<u8> {
    [uleb128(bytes.len()), bytes.to_vec()].concat()
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

fn hex(h: &str) -> Option<Vec<u8>> {
    if h.len() % 2 != 0 {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

/// A decimal or 0x-hex unsigned integer as `width` little-endian bytes.
fn uint(s: &str, width: usize) -> Option<Vec<u8>> {
    let mut le = vec![0u8; width];
    let (digits, radix) = match s.strip_prefix("0x") {
        Some(h) => (h, 16u32),
        None => (s, 10u32),
    };
    if digits.is_empty() {
        return None;
    }
    for c in digits.chars() {
        let d = c.to_digit(radix)?;
        // le = le * radix + d, failing on overflow past `width` bytes.
        let mut carry = d;
        for b in le.iter_mut() {
            let v = *b as u32 * radix + carry;
            *b = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(le)
}

/// A pure input for a PTB.
pub fn pure_input(bytes: &[u8]) -> Value {
    json!({ "kind": "PURE", "pure": base64(bytes) })
}

/// The command's argument for input `i`.
pub fn input_arg(i: usize) -> Value {
    json!({ "kind": "INPUT", "input": i })
}

/// What `send` reports about a transaction: its status, the gas the funder
/// paid, and the objects it created, changed or removed (the gas coin aside).
pub fn summarize(tx: &Value, gas_coin: &str) -> Value {
    let effects = &tx["effects"];
    let gas = &effects["gasUsed"];
    let n = |v: &Value| v.as_str().and_then(|s| s.parse::<i128>().ok()).or_else(|| v.as_i64().map(i128::from)).unwrap_or(0);
    let net = n(&gas["computationCost"]) + n(&gas["storageCost"]) - n(&gas["storageRebate"]);
    let (mut created, mut mutated, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
    for c in effects["changedObjects"].as_array().into_iter().flatten() {
        let id = c["objectId"].as_str().unwrap_or("");
        if id == gas_coin {
            continue;
        }
        let item = json!({ "objectId": id, "type": c["objectType"], "owner": c["outputOwner"] });
        match (c["idOperation"].as_str(), c["outputState"].as_str()) {
            (Some("CREATED"), _) => created.push(item),
            (Some("DELETED"), _) | (_, Some("OUTPUT_OBJECT_STATE_DOES_NOT_EXIST")) => deleted.push(json!({ "objectId": id, "type": c["objectType"] })),
            _ => mutated.push(item),
        }
    }
    let status = &effects["status"];
    let ok = status["success"] == json!(true);
    let mut out = json!({
        "digest": tx["digest"], "status": if ok { 1 } else { 0 },
        "gasUsed": { "net": net.to_string(), "computation": gas["computationCost"], "storage": gas["storageCost"], "rebate": gas["storageRebate"] },
        "created": created, "mutated": mutated, "deleted": deleted,
    });
    if !ok {
        out["error"] = abort_of(&status["error"]);
    }
    out
}

/// The failure of a transaction, with the Move abort's location and code when it aborted.
pub fn abort_of(err: &Value) -> Value {
    let mut out = json!({ "kind": err["kind"], "command": err["command"], "description": err["description"] });
    if let Some(a) = err.get("abort").filter(|a| a.is_object()) {
        let loc = &a["location"];
        out["abort"] = json!({
            "code": a["abortCode"], "package": loc["package"], "module": loc["module"],
            "function": loc["functionName"], "instruction": loc["instruction"],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_with_nested_type_arguments() {
        let t = parse_target("0x2::coin::mint_and_transfer<0xabc::vault::VAULT>").unwrap();
        assert_eq!(t.package, format!("0x{:0>64}", "2"));
        assert_eq!((t.module.as_str(), t.function.as_str()), ("coin", "mint_and_transfer"));
        assert_eq!(t.type_args, ["0xabc::vault::VAULT"]);
        let t = parse_target("0x1::m::f<0x2::coin::Coin<0x2::sui::SUI>, u64>").unwrap();
        assert_eq!(t.type_args, ["0x2::coin::Coin<0x2::sui::SUI>", "u64"]);
        assert!(parse_target("0x2::coin").is_err());
        assert!(parse_target("0x2::coin::f<").is_err());
        assert_eq!(parse_target("transfer(address)").unwrap_err().code, ErrorCode::Usage);
    }

    #[test]
    fn pure_values_encode_as_bcs() {
        assert_eq!(encode_pure(&PureType::U64, "1000000000").unwrap(), 1_000_000_000u64.to_le_bytes());
        assert_eq!(encode_pure(&PureType::U8, "0xff").unwrap(), [0xff]);
        assert!(encode_pure(&PureType::U8, "256").is_err());
        assert_eq!(encode_pure(&PureType::U128, &u128::MAX.to_string()).unwrap(), u128::MAX.to_le_bytes());
        let mut u256 = vec![0u8; 32];
        u256[0] = 1;
        assert_eq!(encode_pure(&PureType::U256, "1").unwrap(), u256);
        assert_eq!(encode_pure(&PureType::Bool, "true").unwrap(), [1]);
        assert_eq!(encode_pure(&PureType::String, "hi").unwrap(), [2, b'h', b'i']);
        let bytes = PureType::Vector(Box::new(PureType::U8));
        assert_eq!(encode_pure(&bytes, "0x0102").unwrap(), [2, 1, 2]);
        assert_eq!(encode_pure(&bytes, "ab").unwrap(), [2, b'a', b'b']);
        let words = PureType::Vector(Box::new(PureType::U16));
        assert_eq!(encode_pure(&words, "[1, 2]").unwrap(), [2, 1, 0, 2, 0]);
        let opt = PureType::Option(Box::new(PureType::U8));
        assert_eq!(encode_pure(&opt, "none").unwrap(), [0]);
        assert_eq!(encode_pure(&opt, "7").unwrap(), [1, 7]);
        let a = encode_pure(&PureType::Address, "0x6").unwrap();
        assert_eq!((a.len(), a[31]), (32, 6));
        assert_eq!(uleb128(300), [0xac, 0x02]);
    }

    #[test]
    fn parameters_map_from_signatures() {
        let cap = json!({ "reference": "MUTABLE", "body": { "type": "DATATYPE", "typeName": "0x0000000000000000000000000000000000000000000000000000000000000002::coin::TreasuryCap" } });
        assert_eq!(param(&cap, "0xef", &[]).unwrap(), Param::Object { id: format!("0x{:0>64}", "ef"), mutable: true });
        let admin = json!({ "reference": "IMMUTABLE", "body": { "type": "DATATYPE", "typeName": "0x1eab::config::AdminCap" } });
        assert!(matches!(param(&admin, "0x1", &[]).unwrap(), Param::Object { mutable: false, .. }));
        let amount = json!({ "body": { "type": "U64" } });
        assert_eq!(param(&amount, "5", &[]).unwrap(), Param::Pure(5u64.to_le_bytes().to_vec()));
        let generic = json!({ "body": { "type": "TYPE_PARAMETER", "typeParameter": 0 } });
        assert_eq!(param(&generic, "9", &["u8".into()]).unwrap(), Param::Pure(vec![9]));
        assert!(matches!(param(&generic, "0x5", &["0x2::coin::Coin<0x2::sui::SUI>".into()]).unwrap(), Param::Object { mutable: true, .. }));
        assert_eq!(param(&generic, "9", &[]).unwrap_err().code, ErrorCode::Usage);
        let name = json!({ "body": { "type": "DATATYPE", "typeName": "0x0000000000000000000000000000000000000000000000000000000000000001::string::String" } });
        assert_eq!(param(&name, "ab", &[]).unwrap(), Param::Pure(vec![2, b'a', b'b']));
        let recv = json!({ "body": { "type": "DATATYPE", "typeName": "0x0000000000000000000000000000000000000000000000000000000000000002::transfer::Receiving" } });
        assert!(matches!(param(&recv, "0x7", &[]).unwrap(), Param::Receiving { .. }));
        let ctx = json!({ "reference": "MUTABLE", "body": { "type": "DATATYPE", "typeName": "0x0000000000000000000000000000000000000000000000000000000000000002::tx_context::TxContext" } });
        assert!(is_tx_context(&ctx) && !is_tx_context(&cap));
    }

    #[test]
    fn effects_summarize_without_the_gas_coin() {
        let tx = json!({
            "digest": "D", "checkpoint": "9",
            "effects": {
                "status": { "success": false, "error": { "kind": "MOVE_ABORT", "command": "0", "description": "MoveAbort(...)",
                    "abort": { "abortCode": "6", "location": { "package": "0xp", "module": "config", "functionName": "check_fee_tier_manager_role", "instruction": 8 } } } },
                "gasUsed": { "computationCost": "1000", "storageCost": "500", "storageRebate": "300" },
                "changedObjects": [
                    { "objectId": "0xgas", "idOperation": "NONE", "outputState": "OUTPUT_OBJECT_STATE_OBJECT_WRITE" },
                    { "objectId": "0xnew", "idOperation": "CREATED", "objectType": "T" },
                    { "objectId": "0xcfg", "idOperation": "NONE", "outputState": "OUTPUT_OBJECT_STATE_OBJECT_WRITE" },
                ],
            },
        });
        let s = summarize(&tx, "0xgas");
        assert_eq!(s["status"], 0);
        assert_eq!(s["gasUsed"]["net"], "1200");
        assert_eq!(s["created"][0]["objectId"], "0xnew");
        assert_eq!(s["mutated"].as_array().unwrap().len(), 1);
        assert_eq!(s["error"]["abort"]["code"], "6");
        assert_eq!(s["error"]["abort"]["function"], "check_fee_tier_manager_role");
    }
}
