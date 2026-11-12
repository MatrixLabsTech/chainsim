//! Balances: the mint behind a token, and native and token balances set to an
//! exact amount and read back.

use chainsim_core::fork::Balance;
use chainsim_core::profile::Profile;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::rpc::{Rpc, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use crate::units::{format_amount, parse_address};

/// A token mint as the fork has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mint {
    /// The profile symbol, or the mint address for a token given by address.
    pub asset: String,
    pub address: String,
    pub decimals: u8,
    /// The program that owns the mint: Token or Token-2022. A token account
    /// for this mint must be owned by the same program.
    pub program: String,
}

/// The program and decimals of a mint account, or None when the account is
/// not a mint of the Token or Token-2022 program.
pub fn mint_of(account: &Value) -> Option<(String, u8)> {
    let program = account["owner"].as_str().filter(|p| *p == TOKEN_PROGRAM || *p == TOKEN_2022_PROGRAM)?;
    let parsed = &account["data"]["parsed"];
    if parsed["type"] != "mint" {
        return None;
    }
    let decimals = parsed["info"]["decimals"].as_u64().and_then(|d| u8::try_from(d).ok())?;
    Some((program.to_string(), decimals))
}

/// Resolves a profile symbol or a mint address, and reads the mint for its
/// owner program and decimals. An address that is not a mint exits 3.
pub async fn resolve_mint(rpc: &Rpc<'_>, profile: &Profile, token: &str) -> Result<Mint> {
    let (asset, address) = match profile.token(token) {
        Some(t) => (t.symbol.clone(), t.address.clone()),
        None => match parse_address(token) {
            Ok(a) => (a.clone(), a),
            Err(_) => {
                let symbols: Vec<_> = profile.tokens.iter().map(|t| t.symbol.as_str()).collect();
                return Err(Error::new(ErrorCode::UnknownToken, format!("no token {token} on {}; symbols: {}", profile.key, symbols.join(", "))).next("pass the mint address instead of a symbol"));
            }
        },
    };
    let not_a_mint = |what: &str| Error::new(ErrorCode::Unsupported, format!("{address} {what} on {}, so it has no token balance to set", profile.name)).next("pass the address of a Token or Token-2022 mint");
    let account = rpc.account(&address).await?.ok_or_else(|| not_a_mint("does not exist"))?;
    let (program, decimals) = mint_of(&account).ok_or_else(|| not_a_mint("is not a token mint"))?;
    Ok(Mint { asset, address, decimals, program })
}

/// Sets a native balance in lamports and reads it back.
pub async fn set_native(rpc: &Rpc<'_>, profile: &Profile, address: &str, lamports: u64) -> Result<Balance> {
    let (symbol, decimals) = profile.native.as_ref().map_or(("SOL".to_string(), 9), |n| (n.symbol.clone(), n.decimals));
    rpc.call("surfnet_setAccount", json!([address, { "lamports": lamports }])).await?;
    let got = rpc.balance(address).await?;
    if got != lamports {
        return Err(Error::new(ErrorCode::ReadbackMismatch, format!("native balance reads {} after setting {}", format_amount(got, decimals), format_amount(lamports, decimals))));
    }
    Ok(Balance { asset: symbol, address: None, decimals, amount: format_amount(got, decimals), raw: got.to_string() })
}

/// Sets a token balance in base units on the owner's token account under the
/// mint's own program, and reads it back. Returns the balance and the token
/// account's address.
pub async fn set_token(rpc: &Rpc<'_>, owner: &str, mint: &Mint, raw: u64) -> Result<(Balance, String)> {
    // Without the program id the engine creates a legacy Token account, also for a Token-2022 mint.
    rpc.call("surfnet_setTokenAccount", json!([owner, mint.address, { "amount": raw }, mint.program])).await?;
    let accounts = rpc.token_accounts(owner, &mint.address).await?;
    let amount_of = |a: &Value| a["account"]["data"]["parsed"]["info"]["tokenAmount"]["amount"].as_str().and_then(|s| s.parse::<u64>().ok());
    let own: Vec<&Value> = accounts.iter().filter(|a| a["account"]["owner"] == mint.program.as_str()).collect();
    let Some(account) = own.iter().find(|a| amount_of(a) == Some(raw)) else {
        let read = own.iter().filter_map(|a| amount_of(a)).map(|v| format_amount(v, mint.decimals)).collect::<Vec<_>>().join(", ");
        return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{} balance reads [{read}] after setting {}", mint.asset, format_amount(raw, mint.decimals))));
    };
    let balance = Balance { asset: mint.asset.clone(), address: Some(mint.address.clone()), decimals: mint.decimals, amount: format_amount(raw, mint.decimals), raw: raw.to_string() };
    Ok((balance, account["pubkey"].as_str().unwrap_or_default().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mint_names_its_program_and_decimals() {
        let usdc = json!({ "owner": TOKEN_PROGRAM, "data": { "program": "spl-token", "parsed": { "type": "mint", "info": { "decimals": 6, "supply": "1" } } } });
        assert_eq!(mint_of(&usdc), Some((TOKEN_PROGRAM.to_string(), 6)));
        let pyusd = json!({ "owner": TOKEN_2022_PROGRAM, "data": { "program": "spl-token-2022", "parsed": { "type": "mint", "info": { "decimals": 6 } } } });
        assert_eq!(mint_of(&pyusd), Some((TOKEN_2022_PROGRAM.to_string(), 6)));
        // A token account, a wallet and a mint-shaped account of another program are not mints.
        let token_account = json!({ "owner": TOKEN_PROGRAM, "data": { "parsed": { "type": "account", "info": {} } } });
        let wallet = json!({ "owner": "11111111111111111111111111111111", "data": ["", "base64"] });
        let other = json!({ "owner": "11111111111111111111111111111111", "data": { "parsed": { "type": "mint", "info": { "decimals": 6 } } } });
        for a in [token_account, wallet, other] {
            assert_eq!(mint_of(&a), None, "{a}");
        }
    }
}
