# <img src="../../assets/chains/arc.svg" height="40" alt=""> Arc mainnet

`arc-mainnet` forks Arc, Circle's L1 where USDC is the gas token. Stock anvil cannot run Arc's
USDC system contract, so chainsim runs Circle's own arc-foundry build of anvil, which does.
Everything not covered here works as on [Ethereum](../ethereum/README.md).

[`profiles/arc-mainnet.json`](../../profiles/arc-mainnet.json) is the source of truth for everything below. Run
`chainsim describe arc-mainnet` to read it from the binary.

## Chain facts

- Chain id: `eip155:5042`. The fork keeps chain id 5042.
- Engine: `chainsim/arc-foundry:v0.8.0-2`, which reports `anvil 1.7.1-dev`.
- Native token: USDC, 18 decimals.
- Upstream: `https://rpc.mainnet.arc.io` by default, or the URL in `CHAINSIM_ARC_MAINNET_RPC`.
- Engine flags: `--retries 20 --fork-retry-backoff 1500 --compute-units-per-second 1000`.

## The arc-foundry engine

Circle publishes arc-foundry as release tarballs, not as an image, so chainsim builds the image
locally from `engines/arc-foundry/Dockerfile` on first use:

- the base is `ubuntu:24.04`, because the tarball binaries need glibc 2.38 or later
- the tarball URL for the host architecture and its SHA-256 come from `engines.lock.json`, and
  the build fails if Circle's published `.sha256` differs from the lock
- the image runs anvil as a non-root user and is tagged `chainsim/arc-foundry:v0.8.0-2`

To build it ahead of the first `up`:

```sh
chainsim pull arc-foundry
```

## USDC: one balance, two views

On Arc the native balance and the USDC ERC-20 at `0x3600000000000000000000000000000000000000`
are the same balance. The native view has 18 decimals and the ERC-20 view has 6, so 100 USDC
is `100e18` natively and `100e6` through `balanceOf`. A native transfer moves the ERC-20
balance, and an ERC-20 `transfer` moves the native balance.

Tokens in the profile:

- `USDC` at `0x3600000000000000000000000000000000000000`, 6 decimals, the native view
- `EURC` at `0xbEf5f6d51CB62b58e6A8f77868681825C6fe21c1`, 6 decimals

`fund --token USDC:<amount>` sets the native balance, since that is where USDC lives, so pass
either `--native` or `--token USDC`, not both. Other tokens go through `anvil_dealERC20` as on
Ethereum:

```sh
chainsim up arc-mainnet
chainsim fund arc-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a --token USDC:100 --token EURC:50
chainsim call arc-mainnet eth_getBalance '["0xd8d244aF69B839999883F9f32a68C7F184012e8a", "latest"]'
chainsim call arc-mainnet eth_call '[{"to": "0x3600000000000000000000000000000000000000", "data": "0x70a08231000000000000000000000000d8d244af69b839999883f9f32a68c7f184012e8a"}, "latest"]'
```

The first `call` returns `0x56bc75e2d63100000` (`100e18`) and the second `0x5f5e100` (`100e6`)
padded to 32 bytes.

## send and set

The same as on Ethereum: `send --as` with `--sig`, `--data` or `--value` (in USDC), and
`set storage`, `set code` and `set balance`. `set balance` writes the native balance, so the
read-back also shows the USDC ERC-20 view. `set token` and `set owner` exit 3.

Gas is paid in USDC, so a sender that cannot pay is topped up in native USDC.

```sh
chainsim set arc-mainnet balance 0xd8d244aF69B839999883F9f32a68C7F184012e8a 25
```

## selftest

The Ethereum checks (`block`, `block-time`, `native-transfer`, `token-transfer` with USDC,
`snapshot-revert`, `upstream-read`) plus one Arc check:

- `dual-usdc-view`: setting the native balance sets the USDC ERC-20 view, and an ERC-20
  transfer moves native balance

```sh
chainsim selftest arc-mainnet
chainsim down arc-mainnet
```

## Limits

The profile's `limits`, as `describe` and the `fork` object print them:

- Transactions confirm on the copy only and never appear on Arc mainnet or its explorer.
- The copy keeps chain id 5042, so a transaction signed for it is also valid on Arc mainnet. Use
  test keys that hold no mainnet assets.
- USDC is the native gas token with 18 decimals, and the USDC ERC-20 is a 6-decimal view of
  the same balance.
- arc-foundry v0.8.0-2 runs Arc's own USDC system contract and precompiles. It is built on an
  older Foundry than v1.8.5, so some newer anvil RPC methods may be missing.
- Accounts and storage the copy has not written are read from the upstream at the fork block
  on first access, so the first read of each account is slower and depends on the upstream's
  rate limit.
- The `--gas-limit` and `--disable-block-gas-limit` options are not available, and there are no
  blob transactions.
- App indexers and explorers read the public chain, so they do not show what happens on the
  copy.
- An old `--block` needs an archive upstream: set `CHAINSIM_ARC_MAINNET_RPC` to an archive
  provider URL to fork far behind the head.
