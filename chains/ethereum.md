# Ethereum mainnet

`ethereum-mainnet` forks Ethereum mainnet with stock Foundry anvil. It is the reference EVM
chain: what this page says about `fund`, `send`, `set` and `selftest` also holds for Arc and
Robinhood Chain unless their pages say otherwise.

`profiles/ethereum-mainnet.json` is the source of truth for everything below. Run
`chainsim describe ethereum-mainnet` to read it from the binary.

## At a glance

- Chain id: `eip155:1`. The fork keeps chain id 1.
- Engine: `ghcr.io/foundry-rs/foundry:v1.8.5` (anvil), pinned by digest in `engines.lock.json`.
- Native token: ETH, 18 decimals.
- Upstream: `https://ethereum-rpc.publicnode.com` by default, or the URL in
  `CHAINSIM_ETHEREUM_MAINNET_RPC`.
- Engine flags: `--retries 20 --fork-retry-backoff 1500 --compute-units-per-second 1000`, so a
  burst of cold reads backs off instead of failing.

## Tokens and fund

- `USDC` at `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48`, 6 decimals
- `WETH` at `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2`, 18 decimals

`fund` sets the native balance with `anvil_setBalance` and an ERC-20 balance with
`anvil_dealERC20`. When the deal does not read back, it searches the token's balance mapping
slot (Solidity and Vyper layouts) and writes that. Any ERC-20 works by address, not only the
symbols above:

```sh
chainsim up ethereum-mainnet
chainsim fund ethereum-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a \
  --native 10 --token USDC:1000 --token 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2:2
```

Every amount is in whole units of the token. `fund` reads each balance back and fails with
`readback_mismatch` if one differs.

## send and set

- `send --as <address>` impersonates any sender, with `--sig "<fn(types)>" <args>...`, raw
  `--data <hex>`, and `--value <amount>` in ETH. When the sender cannot pay for gas, chainsim
  tops it up and reports `topUp`. A mined revert exits 1 (`tx_reverted`) with the decoded
  reason. `--sig` covers flat types (address, bool, uintN, intN, bytesN, bytes, string). Encode
  arrays and tuples elsewhere, for example with `cast calldata`, and pass `--data`.
- `set storage <address> <slot> <value>`, `set code <address> <hex or file>` and
  `set balance <address> <amount>` write state directly and read it back.
- `set token` and `set owner` are not supported on EVM chains (exit 3). Use `fund --token` for
  token balances, and `set storage` for the slot that records an owner.

```sh
chainsim set ethereum-mainnet code 0xd8d244aF69B839999883F9f32a68C7F184012e8a 0x600160005260206000f3
chainsim set ethereum-mainnet storage 0xd8d244aF69B839999883F9f32a68C7F184012e8a 0 42
chainsim status ethereum-mainnet
```

`status` lists every `send` and `set` under `modifications`. The list lives in the fork's
sidecar, so it is gone after `down`, and a `revert` does not shorten it.

## Old blocks need an archive upstream

`--block` forks at a past block instead of the head. The public default keeps only recent
state, so a fork far behind the head fails at `up`:

```sh
chainsim up ethereum-mainnet --name eth-old --block 1000000
```

This exits 5 (`upstream_unavailable`), and `error.next` says to set
`CHAINSIM_ETHEREUM_MAINNET_RPC` to an archive provider URL. With an archive URL in that
variable, the same command starts the fork at block 1000000.

## selftest

`chainsim selftest ethereum-mainnet` runs these checks inside one snapshot and reverts it, so it
leaves no trace:

- `block`: the fork returns its latest block
- `block-time`: the latest block is within 5 minutes of the host clock
- `native-transfer`: an ETH transfer confirms within 10 s and moves the balance
- `token-transfer`: a USDC approve and transfer confirm and move the balance
- `snapshot-revert`: `evm_revert` undoes a transfer made after `evm_snapshot`
- `upstream-read`: a cold account read from the upstream answers within 10 s

```sh
chainsim selftest ethereum-mainnet
chainsim down ethereum-mainnet
```

## Limits

The profile's `limits`, as `describe` and the `fork` object print them:

- Transactions confirm on the copy only and never appear on Ethereum mainnet or its explorers.
- The copy keeps chain id 1, so a transaction signed for it is also valid on mainnet. Use test
  keys that hold no mainnet assets.
- Blocks are mined on demand, not every 12 seconds, and there is no beacon chain, blob data or
  MEV.
- Accounts and storage the copy has not written are read from the upstream at the fork block
  on first access, so the first read of each account is slower and depends on the upstream's
  rate limit.
- The default upstream is a public endpoint that rate-limits bursts. Set
  `CHAINSIM_ETHEREUM_MAINNET_RPC` to a provider URL for heavier runs.
- App indexers and explorers read the public chain, so they do not show what happens on the
  copy.
- An old `--block` needs an archive upstream.
