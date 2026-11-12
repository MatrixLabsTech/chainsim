//! `set <name> mirror kamino`: a Kamino lending position of Solana Mainnet,
//! copied onto a test wallet on the fork. The copy is the source obligation's
//! bytes at the address Kamino derives for the test wallet, with the owner
//! rewritten, plus the farm user state of each farmed reserve it holds.

use chainsim_core::{Error, ErrorCode, Result};
use curve25519_dalek::edwards::CompressedEdwardsY;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::rpc::Rpc;
use crate::send::base64_decode;
use crate::units::{base58_decode, base58_encode, parse_address};
use crate::SolanaAdapter;

/// Kamino Lend.
pub const KLEND: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
/// Kamino Farms, which holds an obligation's stake in a reserve's farm.
pub const FARMS: &str = "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr";

pub type Key = [u8; 32];

const OBLIGATION_LEN: usize = 3344;
/// The obligation's lending market and owner.
const OBLIGATION_MARKET: usize = 32;
const OBLIGATION_OWNER: usize = 64;
/// Eight deposits of 136 bytes and five borrows of 200 bytes, each starting with its reserve.
const DEPOSITS: (usize, usize, usize) = (96, 136, 8);
const BORROWS: (usize, usize, usize) = (1208, 200, 5);
const RESERVE_LEN: usize = 8624;
/// A reserve's farm for collateral and its farm for debt. All zero when it has none.
const RESERVE_FARM_COLLATERAL: usize = 64;
const RESERVE_FARM_DEBT: usize = 96;
/// The mint of the token a reserve lends.
const RESERVE_MINT: usize = 128;
const FARM_USER_LEN: usize = 920;
/// A farm user state's owner, and the obligation it stands for.
const FARM_USER_OWNER: usize = 48;
const FARM_USER_OBLIGATION: usize = 480;

/// An address as its 32 bytes.
pub fn key(address: &str) -> Option<Key> {
    base58_decode(address)?.try_into().ok()
}

fn key_at(data: &[u8], offset: usize) -> Key {
    data[offset..offset + 32].try_into().expect("32 bytes")
}

/// The first 8 bytes of an Anchor account: SHA-256 of "account:<Name>".
pub fn account_discriminator(name: &str) -> [u8; 8] {
    Sha256::digest(format!("account:{name}"))[..8].try_into().expect("8 bytes")
}

/// The program-derived address for the seeds, and its bump: the first hash,
/// counting the bump down from 255, that is not a point of the Ed25519 curve.
pub fn find_program_address(seeds: &[&[u8]], program: &Key) -> Option<(Key, u8)> {
    (0..=255u8).rev().find_map(|bump| {
        let mut hash = Sha256::new();
        for seed in seeds {
            hash.update(seed);
        }
        hash.update([bump]);
        hash.update(program);
        hash.update(b"ProgramDerivedAddress");
        let address: Key = hash.finalize().into();
        CompressedEdwardsY(address).decompress().is_none().then_some((address, bump))
    })
}

/// The seeds of an obligation's address after its owner and market: a tag for
/// its kind, an id, and two keys. A plain obligation has tag 0 and two default
/// keys. The other kinds carry mints, such as collateral and debt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seeds {
    pub tag: u8,
    pub id: u8,
    pub seed1: Key,
    pub seed2: Key,
}

/// The address of an owner's obligation in a market for the seeds.
pub fn obligation_address(owner: &Key, market: &Key, seeds: &Seeds) -> Option<Key> {
    let klend = key(KLEND)?;
    find_program_address(&[&[seeds.tag], &[seeds.id], owner, market, &seeds.seed1, &seeds.seed2], &klend).map(|(address, _)| address)
}

/// The seeds that give `address`, searched over tags 0 to 3, ids 0 to 7, and
/// the default key and `mints` as each of the two keys. None when none does.
pub fn obligation_seeds(address: &Key, owner: &Key, market: &Key, mints: &[Key]) -> Option<Seeds> {
    let mut keys = vec![[0; 32]];
    keys.extend(mints);
    for tag in 0..4 {
        for id in 0..8 {
            for seed1 in &keys {
                for seed2 in &keys {
                    let seeds = Seeds { tag, id, seed1: *seed1, seed2: *seed2 };
                    if obligation_address(owner, market, &seeds).as_ref() == Some(address) {
                        return Some(seeds);
                    }
                }
            }
        }
    }
    None
}

/// The address of the farm user state that stands for an obligation in a farm.
pub fn farm_user_address(farm: &Key, obligation: &Key) -> Option<Key> {
    let farms = key(FARMS)?;
    find_program_address(&[b"user", farm, obligation], &farms).map(|(address, _)| address)
}

/// What a mirror reads from an obligation account.
#[derive(Debug, PartialEq)]
pub struct Obligation {
    pub market: Key,
    pub owner: Key,
    pub deposits: Vec<Key>,
    pub borrows: Vec<Key>,
}

/// The reserves in the slots of one list that are in use.
fn reserves(data: &[u8], (start, step, count): (usize, usize, usize)) -> Vec<Key> {
    (0..count).map(|i| key_at(data, start + step * i)).filter(|k| k != &[0; 32]).collect()
}

/// Reads an obligation account, or None when the bytes are not one.
pub fn parse_obligation(data: &[u8]) -> Option<Obligation> {
    if data.len() != OBLIGATION_LEN || data[..8] != account_discriminator("Obligation") {
        return None;
    }
    Some(Obligation { market: key_at(data, OBLIGATION_MARKET), owner: key_at(data, OBLIGATION_OWNER), deposits: reserves(data, DEPOSITS), borrows: reserves(data, BORROWS) })
}

/// The obligation's bytes with the owner replaced by `wallet`.
pub fn rewrite_obligation(data: &[u8], wallet: &Key) -> Vec<u8> {
    let mut out = data.to_vec();
    out[OBLIGATION_OWNER..OBLIGATION_OWNER + 32].copy_from_slice(wallet);
    out
}

/// A farm user state's bytes for the copy: the obligation it stands for
/// replaced by `copy`, and its owner by `wallet` when the source's owner holds
/// it. None when the account is not the state of `source`.
pub fn rewrite_farm_user(data: &[u8], source: &Key, owner: &Key, copy: &Key, wallet: &Key) -> Option<Vec<u8>> {
    if data.len() != FARM_USER_LEN || data[..8] != account_discriminator("UserState") || &key_at(data, FARM_USER_OBLIGATION) != source {
        return None;
    }
    let mut out = data.to_vec();
    out[FARM_USER_OBLIGATION..FARM_USER_OBLIGATION + 32].copy_from_slice(copy);
    if &key_at(data, FARM_USER_OWNER) == owner {
        out[FARM_USER_OWNER..FARM_USER_OWNER + 32].copy_from_slice(wallet);
    }
    Some(out)
}

/// The byte ranges, as [start, end), where two accounts of one length differ.
pub fn changed_ranges(a: &[u8], b: &[u8]) -> Vec<[usize; 2]> {
    let mut out: Vec<[usize; 2]> = Vec::new();
    for i in (0..a.len().min(b.len())).filter(|&i| a[i] != b[i]) {
        match out.last_mut() {
            Some(last) if last[1] == i => last[1] = i + 1,
            _ => out.push([i, i + 1]),
        }
    }
    out
}

struct Account {
    lamports: u64,
    owner: String,
    data: Vec<u8>,
}

/// An account's raw bytes as the fork has it now, or None when it does not exist.
async fn read(rpc: &Rpc<'_>, address: &str) -> Result<Option<Account>> {
    let v = rpc.call("getAccountInfo", json!([address, { "encoding": "base64", "commitment": "processed" }])).await?;
    let a = &v["value"];
    if a.is_null() {
        return Ok(None);
    }
    match (a["lamports"].as_u64(), a["owner"].as_str(), a["data"][0].as_str().and_then(base64_decode)) {
        (Some(lamports), Some(owner), Some(data)) => Ok(Some(Account { lamports, owner: owner.to_string(), data })),
        _ => Err(Error::new(ErrorCode::RpcError, format!("getAccountInfo of {address} answered {a}"))),
    }
}

/// Writes an account with `surfnet_setAccount` and reads it back.
async fn write(rpc: &Rpc<'_>, address: &str, account: &Account) -> Result<()> {
    let hex: String = account.data.iter().map(|b| format!("{b:02x}")).collect();
    rpc.call("surfnet_setAccount", json!([address, { "lamports": account.lamports, "data": hex, "owner": account.owner, "executable": false }])).await?;
    match read(rpc, address).await? {
        Some(back) if back.data == account.data && back.owner == account.owner && back.lamports == account.lamports => Ok(()),
        _ => Err(Error::new(ErrorCode::ReadbackMismatch, format!("account {address} does not read back as written"))),
    }
}

fn not_mirrorable(what: String) -> Error {
    Error::unsupported(&what, "chainsim call <name> surfnet_setAccount '[\"<address>\", {\"data\": \"<hex>\", \"owner\": \"<program>\"}]' writes any account")
}

/// Copies the Kamino obligation `source` onto the test wallet `to`: the
/// obligation goes to the address derived for the wallet with the owner
/// rewritten, and so does each farm user state of the reserves it holds.
pub async fn kamino(adapter: &SolanaAdapter, name: &str, source: &str, to: &str) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let source = parse_address(source)?;
    let wallet = parse_address(to)?;
    let (source_key, wallet_key) = (key(&source).expect("a parsed address"), key(&wallet).expect("a parsed address"));

    let account = read(&rpc, &source).await?.ok_or_else(|| Error::new(ErrorCode::Usage, format!("{source} does not exist on Solana Mainnet")).next("pass the address of a Kamino obligation account, not of its owner"))?;
    let obligation = (account.owner == KLEND).then(|| parse_obligation(&account.data)).flatten().ok_or_else(|| Error::new(ErrorCode::Usage, format!("{source} is not a Kamino Lend obligation")).next("pass the address of a Kamino obligation account, not of its owner"))?;
    if obligation.owner == wallet_key {
        return Err(Error::new(ErrorCode::Usage, format!("{wallet} already owns {source}")).next("pass a test wallet you hold the key of"));
    }
    // Each position's reserve gives a mint, which may be a seed of the address, and a farm:
    // a deposit stakes in the reserve's collateral farm, a borrow in its debt farm.
    let (mut mints, mut farms) = (Vec::new(), Vec::new());
    for (reserves, farm_at) in [(&obligation.deposits, RESERVE_FARM_COLLATERAL), (&obligation.borrows, RESERVE_FARM_DEBT)] {
        for reserve in reserves {
            let address = base58_encode(reserve);
            let data = read(&rpc, &address).await?.filter(|r| r.owner == KLEND && r.data.len() == RESERVE_LEN).ok_or_else(|| Error::new(ErrorCode::RpcError, format!("reserve {address} of {source} is not a Kamino Lend reserve on the fork")))?.data;
            mints.push(key_at(&data, RESERVE_MINT));
            farms.push((address, key_at(&data, farm_at)));
        }
    }
    let seeds = obligation_seeds(&source_key, &obligation.owner, &obligation.market, &mints)
        .ok_or_else(|| not_mirrorable(format!("set mirror kamino of {source}, whose address no tag from 0 to 3 and id from 0 to 7 gives with the mints it holds now")))?;
    let copy_key = obligation_address(&wallet_key, &obligation.market, &seeds).ok_or_else(|| Error::new(ErrorCode::Internal, "no address derives for the wallet"))?;
    let copy = base58_encode(&copy_key);

    let mut farm_users = Vec::new();
    for (address, farm) in farms {
        if farm == [0; 32] {
            continue;
        }
        let from = farm_user_address(&farm, &source_key).map(|k| base58_encode(&k)).unwrap_or_default();
        // An obligation that never staked in the farm has no state there.
        let Some(mut state) = read(&rpc, &from).await? else { continue };
        let data = rewrite_farm_user(&state.data, &source_key, &obligation.owner, &copy_key, &wallet_key).ok_or_else(|| not_mirrorable(format!("set mirror kamino of {source}, whose farm account {from} is not a farm user state of it")))?;
        let rewritten = changed_ranges(&state.data, &data);
        state.data = data;
        let to = farm_user_address(&farm, &copy_key).map(|k| base58_encode(&k)).unwrap_or_default();
        farm_users.push((json!({ "reserve": address, "farm": base58_encode(&farm), "source": from, "copy": to, "rewritten": rewritten }), state));
    }

    // Every source account is read and checked, so the writes start here.
    for (entry, state) in &farm_users {
        write(&rpc, entry["copy"].as_str().unwrap_or_default(), state).await?;
    }
    let states: Vec<&Value> = farm_users.iter().map(|(entry, _)| entry).collect();
    let copied = Account { lamports: account.lamports, owner: account.owner.clone(), data: rewrite_obligation(&account.data, &wallet_key) };
    write(&rpc, &copy, &copied).await?;

    let slot = rpc.slot().await?;
    let list = |keys: &[Key]| keys.iter().map(|k| base58_encode(k)).collect::<Vec<_>>();
    adapter.record(name, json!({ "kind": "mirror-kamino", "source": source, "obligation": copy, "owner": wallet, "farmUserStates": states.len() })).await?;
    Ok(json!({
        "kind": "mirror",
        "protocol": "kamino",
        "program": KLEND,
        "source": source,
        "sourceOwner": base58_encode(&obligation.owner),
        "obligation": copy,
        "owner": wallet,
        "market": base58_encode(&obligation.market),
        "seeds": { "tag": seeds.tag, "id": seeds.id, "seed1": base58_encode(&seeds.seed1), "seed2": base58_encode(&seeds.seed2) },
        "reserves": { "deposits": list(&obligation.deposits), "borrows": list(&obligation.borrows) },
        "farmUserStates": states,
        "rewritten": [[OBLIGATION_OWNER, OBLIGATION_OWNER + 32]],
        "bytes": account.data.len(),
        "slot": slot,
        "note": chainsim_core::mods::NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(address: &str) -> Key {
        key(address).unwrap()
    }

    /// Addresses and bumps read on Solana Mainnet on 2026-11-12.
    #[test]
    fn program_addresses_match_mainnet() {
        let (klend, farms) = (k(KLEND), k(FARMS));
        let market = k("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF");
        let owner = k("49c6fEcJGmbDt4pGNBajNRWykMHSbzz4cLqQFhJi3ksr");
        let obligation = k("HpcQML7qJ62yXBtbaam94L5HzCnhtEr1qfMqXZTwgS5W");
        // Bump 254: the hash for 255 is a curve point, so a wrong curve test gives another address.
        assert_eq!(find_program_address(&[&[0], &[0], &owner, &market, &[0; 32], &[0; 32]], &klend), Some((obligation, 254)));
        let plain = Seeds { tag: 0, id: 0, seed1: [0; 32], seed2: [0; 32] };
        assert_eq!(obligation_address(&owner, &market, &plain), Some(obligation));
        assert_eq!(obligation_seeds(&obligation, &owner, &market, &[]), Some(plain));
        assert_eq!(find_program_address(&[b"lma", &market], &klend), Some((k("9DrvZvyWh1HuAoZxvYWMvkf2XCzryCpGgHqrMjyDWpmo"), 248)));
        let farm = k("9CinLHLAcMkzs4Ji8pwS2qwyz1LU46A4Ry7BNLGLubxs");
        assert_eq!(find_program_address(&[b"user", &farm, &obligation], &farms), Some((k("96NxMhncGSdMJKqcqdcbAxAXAMdiiNhemNNBUGhVQ3MA"), 254)));
        assert_eq!(farm_user_address(&k("955xWFhSDcDiUgUr4sBRtCpTLiMd4H5uZLAmgtP3R3sX"), &obligation), Some(k("GoL7tAgDtK1tmA7Qyep1rUey4VXSNzUCFdaXWT9ZeHQ7")));
        // A second market and owner, bump 255.
        let jlp_market = k("DxXdAyU3kCjnyggvHmY5nAwg5cRbbmdyX3npfDMjjMek");
        assert_eq!(obligation_address(&k("Dr91VxPi2dyTFyg5hjpFP3murEpWsXzjKXy6joS9hJZX"), &jlp_market, &plain), Some(k("9Z7nH6krHvtEqDDMV5bNj2enPb99qVpu3acB3KeasCxW")));
    }

    /// Obligations of other kinds, read on Solana Mainnet on 2026-11-12.
    #[test]
    fn the_seeds_of_an_obligation_are_found() {
        // A second plain obligation of one owner: id 2.
        let (owner, market) = (k("9X51dLtDsbdoBXPNb8nx5j9YW13X9cQkejN9Q3pBZzMJ"), k("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF"));
        let found = obligation_seeds(&k("5SyVqTUMr11ZdxQJcrWcuVSeRytqD8NeAHZ8egBJRear"), &owner, &market, &[]).unwrap();
        assert_eq!((found.tag, found.id, found.seed1, found.seed2), (0, 2, [0; 32], [0; 32]));
        // Tag 1, with the collateral mint and the debt mint as the two keys, whatever order the mints come in.
        let (owner, market) = (k("3F43GRim9pQLkP3iCCGVdDQwUqu2HFBJLf7Uj1d3hmht"), k("47tfyEG9SsdEnUm9cw5kY9BXngQGqu3LBoop9j5uTAv8"));
        let (collateral, debt) = (k("5Y8NV33Vv7WbnLfq3zBcKSdYPrk7g2KoiQoe7M2tcxp5"), k("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"));
        let address = k("CxpYkhDVcgig6SihtqKvh6ystKairVXau6t42qndQxvF");
        let want = Seeds { tag: 1, id: 0, seed1: collateral, seed2: debt };
        assert_eq!(obligation_seeds(&address, &owner, &market, &[debt, collateral]), Some(want));
        assert_eq!(obligation_address(&owner, &market, &want), Some(address));
        // Without the mints, or for another owner, no seeds give the address.
        assert_eq!(obligation_seeds(&address, &owner, &market, &[]), None);
        assert_eq!(obligation_seeds(&address, &market, &owner, &[debt, collateral]), None);
    }

    fn obligation_bytes() -> Vec<u8> {
        let mut data: Vec<u8> = (0..OBLIGATION_LEN).map(|i| (i % 251) as u8).collect();
        data[..8].copy_from_slice(&account_discriminator("Obligation"));
        // Two deposits and one borrow. The other slots are empty.
        for i in 2..8 {
            data[96 + 136 * i..128 + 136 * i].fill(0);
        }
        for i in 1..5 {
            data[1208 + 200 * i..1240 + 200 * i].fill(0);
        }
        data
    }

    #[test]
    fn an_obligation_is_read_and_only_its_owner_is_rewritten() {
        // The discriminators Kamino's accounts carry on Solana Mainnet.
        assert_eq!(account_discriminator("Obligation"), [168, 206, 141, 106, 88, 76, 172, 167]);
        let data = obligation_bytes();
        let o = parse_obligation(&data).unwrap();
        assert_eq!((o.market, o.owner), (key_at(&data, 32), key_at(&data, 64)));
        assert_eq!(o.deposits, vec![key_at(&data, 96), key_at(&data, 232)]);
        assert_eq!(o.borrows, vec![key_at(&data, 1208)]);

        let wallet = [7u8; 32];
        let copy = rewrite_obligation(&data, &wallet);
        assert_eq!(changed_ranges(&data, &copy), vec![[64, 96]]);
        assert_eq!(parse_obligation(&copy).unwrap(), Obligation { owner: wallet, ..o });

        // Another length, or another account type, is not an obligation.
        assert!(parse_obligation(&data[..OBLIGATION_LEN - 1]).is_none());
        let mut other = data.clone();
        other[0] ^= 1;
        assert!(parse_obligation(&other).is_none());
    }

    #[test]
    fn a_farm_user_state_is_rewritten_for_the_copy() {
        let (source, owner, copy, wallet) = ([1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]);
        let mut data: Vec<u8> = (0..FARM_USER_LEN).map(|i| (i % 249) as u8).collect();
        data[..8].copy_from_slice(&account_discriminator("UserState"));
        data[48..80].copy_from_slice(&owner);
        data[480..512].copy_from_slice(&source);
        let out = rewrite_farm_user(&data, &source, &owner, &copy, &wallet).unwrap();
        assert_eq!(changed_ranges(&data, &out), vec![[48, 80], [480, 512]]);
        assert_eq!((key_at(&out, 48), key_at(&out, 480)), (wallet, copy));

        // An owner that is not the source's owner stays.
        let kept = rewrite_farm_user(&data, &source, &[9; 32], &copy, &wallet).unwrap();
        assert_eq!(changed_ranges(&data, &kept), vec![[480, 512]]);
        // The state of another obligation, or another account, is refused.
        assert!(rewrite_farm_user(&data, &[8; 32], &owner, &copy, &wallet).is_none());
        assert!(rewrite_farm_user(&data[..900], &source, &owner, &copy, &wallet).is_none());
    }

    #[test]
    fn changed_ranges_join_neighbors() {
        assert_eq!(changed_ranges(&[0, 1, 2, 3, 4, 5], &[0, 9, 9, 3, 4, 9]), vec![[1, 3], [5, 6]]);
        assert!(changed_ranges(&[1, 2], &[1, 2]).is_empty());
    }
}
