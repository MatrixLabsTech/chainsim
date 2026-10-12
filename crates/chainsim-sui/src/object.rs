//! `set object`: a Move object's BCS contents decoded by the layouts the fork
//! reports (MovePackageService/GetDatatype), fields changed by dotted path,
//! and the result encoded again. Everything here is pure; the adapter fetches
//! the layouts and calls ForkingService/SetObject.

use std::collections::BTreeMap;

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::parse_address;
use crate::ptb::{encode_pure, split_top_level, uleb128, PureType};

/// A Move type with its type arguments filled in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ty {
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    U256,
    Address,
    Vector(Box<Ty>),
    /// `name` is `0x<64 hex>::module::Name`.
    Datatype { name: String, args: Vec<Ty> },
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Bool => f.write_str("bool"),
            Ty::U8 => f.write_str("u8"),
            Ty::U16 => f.write_str("u16"),
            Ty::U32 => f.write_str("u32"),
            Ty::U64 => f.write_str("u64"),
            Ty::U128 => f.write_str("u128"),
            Ty::U256 => f.write_str("u256"),
            Ty::Address => f.write_str("address"),
            Ty::Vector(t) => write!(f, "vector<{t}>"),
            Ty::Datatype { name, args } if args.is_empty() => f.write_str(&short(name)),
            Ty::Datatype { name, args } => write!(f, "{}<{}>", short(name), args.iter().map(Ty::to_string).collect::<Vec<_>>().join(", ")),
        }
    }
}

/// `0x0000…0002::object::UID` as `0x2::object::UID`, for messages.
fn short(name: &str) -> String {
    let Some((address, rest)) = name.split_once("::") else { return name.to_string() };
    let digits = address.trim_start_matches("0x").trim_start_matches('0');
    if digits.len() <= 4 { format!("0x{}::{rest}", if digits.is_empty() { "0" } else { digits }) } else { name.to_string() }
}

fn framework(n: u8, path: &str) -> String {
    format!("0x{n:0>64}::{path}")
}

/// Parses a type tag such as `0x2::coin::Coin<0x2::sui::SUI>` or `vector<u8>`.
pub fn parse_type(s: &str) -> Result<Ty> {
    let s = s.trim();
    let bad = || Error::new(ErrorCode::Internal, format!("{s:?} is not a Move type"));
    Ok(match s {
        "bool" => Ty::Bool,
        "u8" => Ty::U8,
        "u16" => Ty::U16,
        "u32" => Ty::U32,
        "u64" => Ty::U64,
        "u128" => Ty::U128,
        "u256" => Ty::U256,
        "address" => Ty::Address,
        _ => {
            if let Some(inner) = s.strip_prefix("vector<").and_then(|r| r.strip_suffix('>')) {
                return Ok(Ty::Vector(Box::new(parse_type(inner)?)));
            }
            let (path, args) = match s.find('<') {
                Some(i) => {
                    let inner = s[i..].strip_prefix('<').and_then(|r| r.strip_suffix('>')).ok_or_else(bad)?;
                    (&s[..i], split_top_level(inner).into_iter().map(parse_type).collect::<Result<Vec<_>>>()?)
                }
                None => (s, Vec::new()),
            };
            let parts: Vec<&str> = path.split("::").collect();
            let [address, module, name] = parts[..] else { return Err(bad()) };
            Ty::Datatype { name: format!("{}::{module}::{name}", parse_address(address).map_err(|_| bad())?), args }
        }
    })
}

/// One datatype's layout from GetDatatype: a struct's fields, or an enum's variants.
#[derive(Debug, Clone)]
pub enum Layout {
    Struct(Vec<(String, Value)>),
    Enum(Vec<(String, Vec<(String, Value)>)>),
}

/// Fields in declaration order: `(name, OpenSignatureBody)`.
fn fields_of(list: &Value) -> Vec<(String, Value)> {
    let mut fields: Vec<(u64, String, Value)> =
        list.as_array().into_iter().flatten().map(|f| (f["position"].as_u64().unwrap_or(0), f["name"].as_str().unwrap_or("").to_string(), f["type"].clone())).collect();
    fields.sort_by_key(|(p, _, _)| *p);
    fields.into_iter().map(|(_, n, t)| (n, t)).collect()
}

/// The layout in a GetDatatype reply's `datatype`.
pub fn layout_of(datatype: &Value) -> Layout {
    if datatype["kind"] == json!("ENUM") {
        let mut variants: Vec<(u64, String, Fields)> = datatype["variants"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|v| (v["position"].as_u64().unwrap_or(0), v["name"].as_str().unwrap_or("").to_string(), fields_of(&v["fields"])))
            .collect();
        variants.sort_by_key(|(p, _, _)| *p);
        Layout::Enum(variants.into_iter().map(|(_, n, f)| (n, f)).collect())
    } else {
        Layout::Struct(fields_of(&datatype["fields"]))
    }
}

/// A field's type from its OpenSignatureBody, with the datatype's type arguments substituted.
pub fn ty_of_body(body: &Value, args: &[Ty]) -> Result<Ty> {
    let inner = |i: usize| -> Result<Ty> {
        let b = body["typeParameterInstantiation"].get(i).ok_or_else(|| Error::new(ErrorCode::Internal, format!("a field type has no type argument {i}: {body}")))?;
        ty_of_body(b, args)
    };
    Ok(match body["type"].as_str().unwrap_or("") {
        "BOOL" => Ty::Bool,
        "U8" => Ty::U8,
        "U16" => Ty::U16,
        "U32" => Ty::U32,
        "U64" => Ty::U64,
        "U128" => Ty::U128,
        "U256" => Ty::U256,
        "ADDRESS" => Ty::Address,
        "VECTOR" => Ty::Vector(Box::new(inner(0)?)),
        "TYPE_PARAMETER" => {
            let i = body["typeParameter"].as_u64().unwrap_or(0) as usize;
            args.get(i).cloned().ok_or_else(|| Error::new(ErrorCode::Internal, format!("a field uses type parameter {i}, which the object type does not fill")))?
        }
        "DATATYPE" => {
            let n = body["typeParameterInstantiation"].as_array().map_or(0, Vec::len);
            let name = body["typeName"].as_str().unwrap_or("");
            let Ty::Datatype { name, .. } = parse_type(name)? else { return Err(Error::new(ErrorCode::Internal, format!("{name} is not a datatype"))) };
            Ty::Datatype { name, args: (0..n).map(inner).collect::<Result<_>>()? }
        }
        other => return Err(Error::new(ErrorCode::Internal, format!("unknown field type {other:?}"))),
    })
}

/// Fields in order, as `(name, OpenSignatureBody)`.
type Fields = Vec<(String, Value)>;

/// The layouts a decode needs, by datatype name.
pub type Layouts = BTreeMap<String, Layout>;

/// The datatypes `ty` reaches whose layout is not in `layouts` yet.
pub fn missing(ty: &Ty, layouts: &Layouts) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    walk(ty, layouts, &mut out, &mut seen)?;
    Ok(out)
}

fn walk(ty: &Ty, layouts: &Layouts, out: &mut Vec<String>, seen: &mut std::collections::BTreeSet<Ty>) -> Result<()> {
    if !seen.insert(ty.clone()) {
        return Ok(());
    }
    match ty {
        Ty::Vector(t) => walk(t, layouts, out, seen),
        Ty::Datatype { name, args } => {
            for a in args {
                walk(a, layouts, out, seen)?;
            }
            match layouts.get(name) {
                None => {
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                    Ok(())
                }
                Some(layout) => {
                    for (_, body) in all_fields(layout) {
                        walk(&ty_of_body(body, args)?, layouts, out, seen)?;
                    }
                    Ok(())
                }
            }
        }
        _ => Ok(()),
    }
}

fn all_fields(layout: &Layout) -> Vec<&(String, Value)> {
    match layout {
        Layout::Struct(f) => f.iter().collect(),
        Layout::Enum(v) => v.iter().flat_map(|(_, f)| f.iter()).collect(),
    }
}

/// A decoded value with its type, so a part can be replaced and the whole encoded again.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub ty: Ty,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A primitive's BCS bytes.
    Leaf(Vec<u8>),
    Vector(Vec<Node>),
    Struct(Vec<(String, Node)>),
    Enum { tag: usize, variant: String, fields: Vec<(String, Node)> },
}

fn layout_err(why: String) -> Error {
    Error::new(ErrorCode::Internal, format!("the object's contents do not match its layout: {why}"))
}

/// Decodes all of `bytes` as `ty`.
pub fn decode(ty: &Ty, bytes: &[u8], layouts: &Layouts) -> Result<Node> {
    let mut pos = 0;
    let node = decode_at(ty, bytes, &mut pos, layouts)?;
    if pos != bytes.len() {
        return Err(layout_err(format!("{} bytes left after {ty}", bytes.len() - pos)));
    }
    Ok(node)
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let out = bytes.get(*pos..*pos + n).ok_or_else(|| layout_err(format!("ran out of bytes at offset {pos}")))?;
    *pos += n;
    Ok(out)
}

fn read_uleb(bytes: &[u8], pos: &mut usize) -> Result<usize> {
    let (mut n, mut shift) = (0usize, 0);
    loop {
        let b = take(bytes, pos, 1)?[0];
        n |= ((b & 0x7f) as usize).checked_shl(shift).ok_or_else(|| layout_err("a length is too large".into()))?;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
    }
}

fn decode_at(ty: &Ty, bytes: &[u8], pos: &mut usize, layouts: &Layouts) -> Result<Node> {
    let leaf = |n: usize, pos: &mut usize| -> Result<Kind> { Ok(Kind::Leaf(take(bytes, pos, n)?.to_vec())) };
    let kind = match ty {
        Ty::Bool | Ty::U8 => leaf(1, pos)?,
        Ty::U16 => leaf(2, pos)?,
        Ty::U32 => leaf(4, pos)?,
        Ty::U64 => leaf(8, pos)?,
        Ty::U128 => leaf(16, pos)?,
        Ty::U256 | Ty::Address => leaf(32, pos)?,
        Ty::Vector(t) => {
            let n = read_uleb(bytes, pos)?;
            Kind::Vector((0..n).map(|_| decode_at(t, bytes, pos, layouts)).collect::<Result<_>>()?)
        }
        Ty::Datatype { name, args } => {
            let layout = layouts.get(name).ok_or_else(|| Error::new(ErrorCode::Internal, format!("no layout for {name}")))?;
            let fields = |list: &[(String, Value)], pos: &mut usize| -> Result<Vec<(String, Node)>> {
                list.iter().map(|(n, body)| Ok((n.clone(), decode_at(&ty_of_body(body, args)?, bytes, pos, layouts)?))).collect()
            };
            match layout {
                Layout::Struct(list) => Kind::Struct(fields(list, pos)?),
                Layout::Enum(variants) => {
                    let tag = read_uleb(bytes, pos)?;
                    let (variant, list) = variants.get(tag).ok_or_else(|| layout_err(format!("{ty} has no variant {tag}")))?;
                    Kind::Enum { tag, variant: variant.clone(), fields: fields(list, pos)? }
                }
            }
        }
    };
    Ok(Node { ty: ty.clone(), kind })
}

/// The BCS bytes of a node.
pub fn encode(node: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(node, &mut out);
    out
}

fn encode_into(node: &Node, out: &mut Vec<u8>) {
    match &node.kind {
        Kind::Leaf(b) => out.extend(b),
        Kind::Vector(items) => {
            out.extend(uleb128(items.len()));
            items.iter().for_each(|n| encode_into(n, out));
        }
        Kind::Struct(fields) => fields.iter().for_each(|(_, n)| encode_into(n, out)),
        Kind::Enum { tag, fields, .. } => {
            out.extend(uleb128(*tag));
            fields.iter().for_each(|(_, n)| encode_into(n, out));
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn le_uint(bytes: &[u8]) -> String {
    // Little-endian bytes to decimal, for widths up to 256 bits.
    let mut digits = vec![0u64];
    for &b in bytes.iter().rev() {
        let mut carry = b as u64;
        for d in digits.iter_mut() {
            let v = *d * 256 + carry;
            *d = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        while carry > 0 {
            digits.push(carry % 1_000_000_000);
            carry /= 1_000_000_000;
        }
    }
    let mut s = digits.last().unwrap().to_string();
    for d in digits.iter().rev().skip(1) {
        s.push_str(&format!("{d:09}"));
    }
    s
}

/// A node as JSON: integers from u64 up as decimal strings, IDs and UIDs as
/// addresses, strings as text, Option as null or its value, vector<u8> as hex.
pub fn to_json(node: &Node) -> Value {
    if let Ty::Datatype { name, .. } = &node.ty {
        if *name == framework(2, "object::UID") || *name == framework(2, "object::ID") {
            return json!(hex(&encode(node)));
        }
        if *name == framework(1, "string::String") || *name == framework(1, "ascii::String") {
            let bytes = encode(node);
            let mut pos = 0;
            let _ = read_uleb(&bytes, &mut pos);
            return match std::str::from_utf8(&bytes[pos..]) {
                Ok(s) => json!(s),
                Err(_) => json!(hex(&bytes[pos..])),
            };
        }
        if *name == framework(1, "option::Option") {
            if let Kind::Struct(f) = &node.kind {
                if let Some((_, Node { kind: Kind::Vector(items), .. })) = f.first() {
                    return items.first().map_or(Value::Null, to_json);
                }
            }
        }
    }
    match &node.kind {
        Kind::Leaf(b) => match node.ty {
            Ty::Bool => json!(b[0] != 0),
            Ty::U8 | Ty::U16 | Ty::U32 => json!(le_uint(b).parse::<u64>().unwrap_or(0)),
            Ty::Address => json!(hex(b)),
            _ => json!(le_uint(b)),
        },
        Kind::Vector(items) if matches!(node.ty, Ty::Vector(ref t) if **t == Ty::U8) => {
            json!(hex(&items.iter().map(|n| if let Kind::Leaf(b) = &n.kind { b[0] } else { 0 }).collect::<Vec<_>>()))
        }
        Kind::Vector(items) => Value::Array(items.iter().map(to_json).collect()),
        Kind::Struct(fields) => Value::Object(fields.iter().map(|(n, v)| (n.clone(), to_json(v))).collect()),
        Kind::Enum { variant, fields, .. } => {
            let mut m: serde_json::Map<String, Value> = fields.iter().map(|(n, v)| (n.clone(), to_json(v))).collect();
            m.insert("@variant".into(), json!(variant));
            Value::Object(m)
        }
    }
}

/// The pure type a field takes its new value as, or None when it is a struct
/// that must be set field by field.
fn pure_of(ty: &Ty) -> Option<PureType> {
    Some(match ty {
        Ty::Bool => PureType::Bool,
        Ty::U8 => PureType::U8,
        Ty::U16 => PureType::U16,
        Ty::U32 => PureType::U32,
        Ty::U64 => PureType::U64,
        Ty::U128 => PureType::U128,
        Ty::U256 => PureType::U256,
        Ty::Address => PureType::Address,
        Ty::Vector(t) => PureType::Vector(Box::new(pure_of(t)?)),
        Ty::Datatype { name, args } => {
            if *name == framework(1, "string::String") || *name == framework(1, "ascii::String") {
                PureType::String
            } else if *name == framework(2, "object::ID") {
                PureType::Address
            } else if *name == framework(1, "option::Option") {
                PureType::Option(Box::new(pure_of(args.first()?)?))
            } else {
                return None;
            }
        }
    })
}

/// The names a node's parts go by: field names, or vector indexes.
fn part_names(node: &Node) -> Vec<String> {
    match &node.kind {
        Kind::Struct(f) | Kind::Enum { fields: f, .. } => f.iter().map(|(n, _)| n.clone()).collect(),
        Kind::Vector(items) => (0..items.len()).map(|i| i.to_string()).collect(),
        Kind::Leaf(_) => Vec::new(),
    }
}

/// Sets the field at a dotted `path` (field names, or indexes into a vector) to `value`.
pub fn set_path(root: &mut Node, path: &str, value: &str, layouts: &Layouts) -> Result<()> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(Error::new(ErrorCode::Usage, format!("{path:?} is not a field path; use names joined by dots, such as config.fee")));
    }
    if segments[0] == "id" && matches!(&root.ty, Ty::Datatype { .. }) {
        if let Kind::Struct(f) = &root.kind {
            if f.first().is_some_and(|(n, node)| n == "id" && matches!(&node.ty, Ty::Datatype { name, .. } if *name == framework(2, "object::UID"))) {
                return Err(Error::new(ErrorCode::Usage, "the object's own id cannot change"));
            }
        }
    }
    let mut node = root;
    let mut walked = Vec::new();
    for seg in &segments {
        let names = part_names(node);
        let here = if walked.is_empty() { "the object".to_string() } else { walked.join(".") };
        let child = match &mut node.kind {
            Kind::Struct(f) | Kind::Enum { fields: f, .. } => f.iter_mut().find(|(n, _)| n == seg).map(|(_, n)| n),
            Kind::Vector(items) => seg.parse::<usize>().ok().and_then(|i| items.get_mut(i)),
            Kind::Leaf(_) => None,
        };
        node = child.ok_or_else(|| {
            let has = if names.is_empty() { "no fields".to_string() } else { names.join(", ") };
            Error::new(ErrorCode::Usage, format!("{here} has no field {seg:?}; it has {has}"))
        })?;
        walked.push(*seg);
    }
    let ty = node.ty.clone();
    let pure = pure_of(&ty).ok_or_else(|| {
        Error::new(ErrorCode::Usage, format!("{path} is a {ty}, which is set field by field: {}", part_names(node).iter().map(|n| format!("{path}.{n}")).collect::<Vec<_>>().join(", ")))
    })?;
    let bytes = encode_pure(&pure, value).map_err(|e| Error::new(ErrorCode::Usage, format!("{path} is a {ty}: {}", e.message)))?;
    *node = decode(&ty, &bytes, layouts)?;
    Ok(())
}

/// Splits `--field name=value`.
pub fn split_field(arg: &str) -> Result<(&str, &str)> {
    arg.split_once('=').filter(|(n, _)| !n.trim().is_empty()).map(|(n, v)| (n.trim(), v)).ok_or_else(|| Error::new(ErrorCode::Usage, format!("--field {arg:?} is not name=value, such as --field protocol_fee_rate=1234")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(t: &str) -> Value {
        json!({ "type": t })
    }

    fn dt(name: &str, args: Vec<Value>) -> Value {
        json!({ "type": "DATATYPE", "typeName": name, "typeParameterInstantiation": args })
    }

    fn st(fields: &[(&str, Value)]) -> Layout {
        Layout::Struct(fields.iter().map(|(n, b)| (n.to_string(), b.clone())).collect())
    }

    /// UID, ID, String, Option and a Config<T> { id, fee: u64, name: String, admin: Option<address>, tiers: vector<Tier>, extra: T }.
    fn layouts() -> Layouts {
        let mut l = Layouts::new();
        l.insert(framework(2, "object::UID"), st(&[("id", dt("0x2::object::ID", vec![]))]));
        l.insert(framework(2, "object::ID"), st(&[("bytes", body("ADDRESS"))]));
        l.insert(framework(1, "string::String"), st(&[("bytes", json!({ "type": "VECTOR", "typeParameterInstantiation": [body("U8")] }))]));
        l.insert(framework(1, "option::Option"), st(&[("vec", json!({ "type": "VECTOR", "typeParameterInstantiation": [json!({ "type": "TYPE_PARAMETER" })] }))]));
        l.insert(format!("0x{:0>64}::m::Tier", "abc"), st(&[("size", body("U32")), ("rate", body("U64"))]));
        l.insert(
            format!("0x{:0>64}::m::Config", "abc"),
            st(&[
                ("id", dt("0x2::object::UID", vec![])),
                ("fee", body("U64")),
                ("name", dt("0x1::string::String", vec![])),
                ("admin", dt("0x1::option::Option", vec![body("ADDRESS")])),
                ("tiers", json!({ "type": "VECTOR", "typeParameterInstantiation": [dt("0xabc::m::Tier", vec![])] })),
                ("extra", json!({ "type": "TYPE_PARAMETER", "typeParameter": 0 })),
            ]),
        );
        l.insert(
            format!("0x{:0>64}::m::State", "abc"),
            Layout::Enum(vec![("Off".into(), vec![]), ("On".into(), vec![("level".into(), body("U8"))])]),
        );
        l
    }

    fn config_bytes() -> Vec<u8> {
        let mut b = vec![0x11u8; 32]; // id
        b.extend(2000u64.to_le_bytes()); // fee
        b.extend([2, b'h', b'i']); // name
        b.extend([0]); // admin: none
        b.extend([1]); // one tier
        b.extend(10u32.to_le_bytes());
        b.extend(500u64.to_le_bytes());
        b.extend([1, 7]); // extra: State::On { level: 7 }
        b
    }

    #[test]
    fn types_parse_from_tags() {
        let t = parse_type("0xabc::m::Config<0xabc::m::State>").unwrap();
        assert_eq!(t, Ty::Datatype { name: format!("0x{:0>64}::m::Config", "abc"), args: vec![Ty::Datatype { name: format!("0x{:0>64}::m::State", "abc"), args: vec![] }] });
        assert_eq!(parse_type("vector<u8>").unwrap(), Ty::Vector(Box::new(Ty::U8)));
        assert_eq!(parse_type("0x2::coin::Coin<0x2::sui::SUI>").unwrap().to_string(), "0x2::coin::Coin<0x2::sui::SUI>");
        assert_eq!(parse_type(&format!("0x{}::m::T", "ab".repeat(32))).unwrap().to_string(), format!("0x{}::m::T", "ab".repeat(32)));
        assert!(parse_type("0x2::coin").is_err());
    }

    #[test]
    fn layouts_list_what_is_missing() {
        let all = layouts();
        let ty = parse_type("0xabc::m::Config<0xabc::m::State>").unwrap();
        assert!(missing(&ty, &all).unwrap().is_empty());
        let mut some = all.clone();
        some.remove(&framework(1, "string::String"));
        some.remove(&format!("0x{:0>64}::m::Tier", "abc"));
        let mut m = missing(&ty, &some).unwrap();
        m.sort();
        assert_eq!(m, [framework(1, "string::String"), format!("0x{:0>64}::m::Tier", "abc")]);
    }

    #[test]
    fn contents_decode_and_encode_back_unchanged() {
        let l = layouts();
        let ty = parse_type("0xabc::m::Config<0xabc::m::State>").unwrap();
        let node = decode(&ty, &config_bytes(), &l).unwrap();
        assert_eq!(encode(&node), config_bytes());
        let j = to_json(&node);
        assert_eq!(j["id"], json!(format!("0x{}", "11".repeat(32))));
        assert_eq!(j["fee"], "2000");
        assert_eq!(j["name"], "hi");
        assert_eq!(j["admin"], Value::Null);
        assert_eq!(j["tiers"][0], json!({ "size": 10, "rate": "500" }));
        assert_eq!(j["extra"], json!({ "@variant": "On", "level": 7 }));
        assert!(decode(&ty, &config_bytes()[..40], &l).is_err());
        assert!(decode(&ty, &[config_bytes(), vec![0]].concat(), &l).is_err());
    }

    #[test]
    fn fields_set_by_dotted_path() {
        let l = layouts();
        let ty = parse_type("0xabc::m::Config<0xabc::m::State>").unwrap();
        let mut node = decode(&ty, &config_bytes(), &l).unwrap();
        set_path(&mut node, "fee", "1234", &l).unwrap();
        set_path(&mut node, "name", "cetus", &l).unwrap();
        set_path(&mut node, "admin", "0x5", &l).unwrap();
        set_path(&mut node, "tiers.0.rate", "0x10", &l).unwrap();
        set_path(&mut node, "extra.level", "9", &l).unwrap();
        let j = to_json(&node);
        assert_eq!((j["fee"].as_str(), j["name"].as_str()), (Some("1234"), Some("cetus")));
        assert_eq!(j["admin"], json!(format!("0x{:0>64}", "5")));
        assert_eq!(j["tiers"][0]["rate"], "16");
        assert_eq!(j["extra"]["level"], 9);
        // The new bytes decode again under the same layout.
        assert_eq!(decode(&ty, &encode(&node), &l).unwrap(), node);
    }

    #[test]
    fn bad_field_paths_are_refused_with_what_exists() {
        let l = layouts();
        let ty = parse_type("0xabc::m::Config<0xabc::m::State>").unwrap();
        let mut node = decode(&ty, &config_bytes(), &l).unwrap();
        let e = set_path(&mut node, "fees", "1", &l).unwrap_err();
        assert_eq!(e.code, ErrorCode::Usage);
        assert!(e.message.contains("fee, name"), "{}", e.message);
        let e = set_path(&mut node, "tiers.0", "1", &l).unwrap_err();
        assert!(e.message.contains("tiers.0.size"), "{}", e.message);
        assert!(set_path(&mut node, "tiers.3.rate", "1", &l).is_err());
        assert!(set_path(&mut node, "fee", "-1", &l).is_err());
        assert!(set_path(&mut node, "fee..x", "1", &l).is_err());
        assert!(set_path(&mut node, "id", "0x1", &l).unwrap_err().message.contains("own id"));
        assert_eq!(split_field("a.b=1").unwrap(), ("a.b", "1"));
        assert_eq!(split_field("s=x=y").unwrap(), ("s", "x=y"));
        assert!(split_field("=1").is_err() && split_field("fee").is_err());
    }

    #[test]
    fn wide_integers_render_in_decimal() {
        assert_eq!(le_uint(&u128::MAX.to_le_bytes()), u128::MAX.to_string());
        let mut max = [0xffu8; 32].to_vec();
        assert_eq!(le_uint(&max), "115792089237316195423570985008687907853269984665640564039457584007913129639935");
        max.fill(0);
        assert_eq!(le_uint(&max), "0");
    }
}
