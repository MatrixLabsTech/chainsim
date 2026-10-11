# Robinhood Chain mainnet

`robinhood-mainnet` forks Robinhood Chain, an Arbitrum Nitro L2 with gas in ETH. It runs the
same Foundry anvil as [Ethereum](ethereum.md), which handles Arbitrum's ArbSys, and chainsim
installs a mock for the one precompile anvil cannot answer, ArbGasInfo. Everything not covered
here works as on Ethereum.

`profiles/robinhood-mainnet.json` is the source of truth for everything below. Run
`chainsim describe robinhood-mainnet` to read it from the binary.

## Chain facts

- Chain id: `eip155:4663`. The fork keeps chain id 4663.
- Engine: `ghcr.io/foundry-rs/foundry:v1.8.5` (anvil). v1.8.3 or later is needed for ArbSys.
- Native token: ETH, 18 decimals.
- Upstream: `https://rpc.mainnet.chain.robinhood.com` by default, documented as rate-limited,
  or the URL in `CHAINSIM_ROBINHOOD_MAINNET_RPC`.
- Engine flags: `--retries 20 --fork-retry-backoff 1500 --compute-units-per-second 1000`.

## Tokens and fund

The profile's stablecoin is USDG, not USDC:

- `USDG` at `0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168`, 6 decimals
- `WETH` at `0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73`, 18 decimals

`fund` works as on Ethereum:

```sh
chainsim up robinhood-mainnet
chainsim fund robinhood-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a --native 1 --token USDG:500
```

## ArbSys and the ArbGasInfo mock

- anvil itself answers ArbSys at `0x64`: `arbBlockNumber()` returns the Arbitrum block number,
  and `block.number` keeps Arbitrum semantics.
- ArbGasInfo at `0x6C` reverts on stock anvil. At `up`, chainsim installs a mock there with
  `anvil_setCode`, from `profiles/assets/arbgasinfo-mock.hex` (the profile's `overrides`). The
  mock returns fixed values measured on Robinhood Chain at block 85104168 on 2026-10-10, for
  example `getL1BaseFeeEstimate()` 255074 and `getMinimumGasPrice()` 20000000. Gas prices do
  not move with the chain, and functions the mock does not list revert.

```sh
chainsim call robinhood-mainnet eth_call '[{"to": "0x0000000000000000000000000000000000000064", "data": "0xa3b1b31d"}, "latest"]'
chainsim call robinhood-mainnet eth_call '[{"to": "0x000000000000000000000000000000000000006C", "data": "0xf5d6ded7"}, "latest"]'
```

The first returns the fork's Arbitrum block number, the second 255074 (`0x3e462`), padded to
32 bytes.

### Refreshing the mock values

The values are a dated snapshot. To refresh them:

1. Read each getter from the live chain with the pinned Foundry image, for example:

   ```sh
   docker run --rm --entrypoint cast ghcr.io/foundry-rs/foundry:v1.8.5 \
     call 0x000000000000000000000000000000000000006C "getL1BaseFeeEstimate()(uint256)" \
     --rpc-url https://rpc.mainnet.chain.robinhood.com
   ```

2. Put the new values, block and date in `profiles/assets/src/ArbGasInfoMock.sol`.
3. Rebuild the runtime code with the command in that file's header and write it, without a
   newline, to `profiles/assets/arbgasinfo-mock.hex`:

   ```sh
   docker run --rm -v "$PWD/profiles/assets/src":/in:ro --entrypoint sh \
     ghcr.io/foundry-rs/foundry:v1.8.5 -c 'cd /tmp && mkdir -p b/src && cd b && \
     cp /in/ArbGasInfoMock.sol src/ && forge inspect src/ArbGasInfoMock.sol:ArbGasInfoMock \
     deployedBytecode --use 0.8.30 --optimize'
   ```

4. Update the date, block and example values in the profile's `overrides` note and `limits`,
   and `MOCK_L1_BASE_FEE` in `crates/chainsim-evm`, which the `arbgasinfo` selftest checks.

## send and set

The same as on Ethereum: `send --as` with `--sig`, `--data` or `--value` (in ETH), and
`set storage`, `set code` and `set balance`. `set token` and `set owner` exit 3. A `set code`
at `0x6C` replaces the mock for the rest of the fork's life.

## selftest

The Ethereum checks (`block`, `block-time`, `native-transfer`, `token-transfer` with USDG,
`snapshot-revert`, `upstream-read`) plus one Arbitrum check:

- `arbgasinfo`: `getL1BaseFeeEstimate()` at `0x6C` returns the mock value and
  `arbBlockNumber()` at `0x64` answers

```sh
chainsim selftest robinhood-mainnet
chainsim down robinhood-mainnet
```

## Limits

The profile's `limits`, as `describe` and the `fork` object print them:

- Transactions confirm on the copy only and never appear on Robinhood Chain or its explorers.
- The copy keeps chain id 4663, so a transaction signed for it is also valid on Robinhood
  Chain. Use test keys that hold no mainnet assets.
- anvil answers ArbSys at `0x64` with Arbitrum block numbers, and mines blocks on demand
  instead of following the sequencer.
- ArbGasInfo at `0x6C` is a chainsim mock with fixed values measured on 2026-10-10 at block
  85104168, so gas prices do not move with the chain. Functions outside the common pricing
  getters revert.
- There is no L1 data fee, so gas costs and estimates on the copy are lower than on the chain.
  Retryable tickets and L1-to-L2 address aliasing do not exist on the copy.
- Accounts and storage the copy has not written are read from the upstream at the fork block
  on first access, so the first read of each account is slower and depends on the upstream's
  rate limit.
- The default upstream is rate-limited. Set `CHAINSIM_ROBINHOOD_MAINNET_RPC` to a provider URL
  for heavier runs.
- An old `--block` needs an archive upstream.
- App indexers and explorers read the public chain, so they do not show what happens on the
  copy.
