# Adding a chain

An EVM chain that stock anvil can fork takes a new profile, `profiles/<chain>.json`, and no new
code: chain id, engine, upstream URL and its environment variable, tokens, overrides (such as
Robinhood's ArbGasInfo mock), warm-up transactions, selftest extras and limits. Copy
`profiles/ethereum-mainnet.json`, then add a `chains/<chain>/README.md` that says what differs.
A new chain family needs an adapter crate that implements the `Adapter` trait in
`crates/chainsim-core`, as `crates/chainsim-sui` does for Sui.
