# Aptos

chainsim runs a private copy of Aptos Mainnet in Docker as the chain `aptos-mainnet`. This page
covers what differs from the other chains: the engine, how each command works on Aptos, and what
the copy cannot do. The commands and their JSON output are the same as on every chain. The
[README](../../README.md) covers the shared workflow.

The limits listed here are a summary. The source of truth is the `limits` list in
[`profiles/aptos-mainnet.json`](../../profiles/aptos-mainnet.json), which
`chainsim describe aptos-mainnet` and every fork object report word for word.

## What chainsim solves here

- Aptos has no anvil and no fork node. The Aptos CLI can run transactions against forked mainnet
  state in a simulation session, a directory that records every state change, but it is a
  command line tool with no server. chainsim runs the CLI in a container and drives each
  operation through it.
- Acting as an account without its key fails on Aptos with `INVALID_AUTH_KEY`. `send --as`
  writes the fork's control key into the sender's `Account` resource on the copy first, so the
  CLI can sign for any address.
- Funding a test wallet with USDC means finding a holder and moving the tokens. `fund --token`
  transfers USDC from a large mainnet holder on the copy and reads the balance back.

## Maturity: experimental

`describe` and the fork object report `"maturity": "experimental"`. Aptos marks simulation
sessions as beta and subject to change, and the copy differs from Aptos Mainnet in ways the other
forks do not:

- The copy serves no REST API, so SDKs, wallets and browser dApps cannot connect to it. The fork
  object's `rpcUrl` is `null`, and `--publish` exits 3. Agents read and change the copy through
  `chainsim call`, `send`, `fund` and `set`.
- Every transaction and read runs one at a time through the CLI, which loads and rewrites the
  session files on each call, so runs are slow and cannot run in parallel.
- Aptos signatures bind chain id 1, so a transaction signed for the copy is also valid on Aptos
  Mainnet for that key and sequence number. Use test keys that hold no mainnet assets.

## Engine

- The engine is the Aptos CLI 9.6.0. The first `up` builds the image `chainsim/aptos-cli:9.6.0`
  from `engines/aptos-cli/Dockerfile`, and `chainsim pull aptos-cli` builds it ahead of time. The
  build downloads the official Linux zip for the host (x86_64 or aarch64) from the
  `aptos-cli-v9.6.0` release of aptos-core and checks it against the sha256 in
  `engines.lock.json`. The release publishes no `.sha256` file, so the lock's value is the only
  check. A sha256 that differs from the lock stops the build.
- The image is Debian bookworm-slim with `ca-certificates` and `libssl3`, which the binary links.
  A cold `up` that built the image became ready in 18 s on arm64 (measured on 2026-11-01).
- The engine container idles. Each operation is one CLI call on the session in the fork volume at
  `/data`, serialized with `flock`, since every call loads and rewrites the session files.
- The engine runs as uid 10001, not root. `up` creates the fork volume owned by that user. The
  engine and its control sidecar run with a read-only root filesystem, every Linux capability
  dropped and `no-new-privileges`, and neither publishes a port.

## License

aptos-core and the Aptos CLI are under the
[Innovation-Enabling Source Code License](https://github.com/aptos-labs/aptos-core/blob/aptos-cli-v9.6.0/LICENSE).
It allows non-production and non-commercial use, lists further production uses in an Additional
Use Grant, and turns each version into Apache 2.0 four years after its release. chainsim
redistributes nothing of the CLI, since each host downloads the official release. Whether a use
is allowed under that license is for the user to check.

## Upstream

- The copy reads Aptos Mainnet from the Aptos Labs REST API `https://api.mainnet.aptoslabs.com/v1`
  by default. `CHAINSIM_APTOS_MAINNET_REST` replaces it with a provider REST URL, and
  `CHAINSIM_APTOS_API_KEY` passes an API key to the CLI and to the sidecar's ledger reads.
- Without a key, all requests from one IP share a budget of 40,000 compute units per 5 minutes.
  Every CLI call reads the framework state it needs from the upstream again, so one `selftest`
  run used most of that budget (measured on 2026-11-01). Set an API key for any run beyond a few
  operations. A key is free from Aptos Labs' developer portal, https://geomi.dev. The limit is
  reported as exit 5 with `rate_limited`, also when it fails `selftest` checks.
- The public fullnode keeps about the last 150 million versions. An older `--block` exits 5 with
  advice to use an archive upstream.
- State the copy has not touched is read from the upstream on first access, so the first read of
  each resource is slower.
- The CLI keeps the upstream URL and API key in the session's `config.json` on the fork volume.
  chainsim never prints them, and `down` removes the volume.

## Commands

| Command | On Aptos |
| --- | --- |
| `up [--block <version>]` | Forks at the given ledger version, or the head. The fork point kind is `version`. |
| `fund --native <APT>` | Raises the APT balance with the CLI's direct store write, then reads it back. |
| `fund --token USDC:<amount>` | Raises the USDC balance with a transfer from the profile's holder, sent by impersonation. |
| `send --as <address> --to <function> <args>...` | Runs an entry function as any address. |
| `set balance` and `set token` | Raise an APT or USDC balance, as `fund` does. `set token --from <holder>` names another holder. |
| `set resource <address> <struct tag> --bcs <hex>` | Replaces a resource that is stored on its own. |
| `snapshot` and `revert` | Copy the session directory on the fork volume and back. |
| `time --advance <s>` or `--to <unix s>` | Runs a block with the new timestamp. |
| `mine [--blocks <n>]` | Runs up to 100 blocks, each 1 microsecond later. |
| `call <name> view\|resource\|ledger` | Reads the copy. |
| `selftest` | Checks the chain id, a funding, a transfer, a block and a USDC move. |

`fund`, `set balance` and `set token` only raise a balance. An address that already holds more
than the amount asked is refused with exit 3. `fund` refuses, with exit 7, an address that has
sent a transaction on Aptos Mainnet.

### send --as

`--to` names a Move entry function, with its type arguments in angle brackets. The arguments
follow it in the Aptos CLI's `<type>:<value>` form:

```sh
chainsim send aptos-mainnet --as 0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b \
  --to '0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>' \
  address:0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b \
  address:<receiver> u64:1000000
```

- Before the transaction, chainsim replaces the authentication key in the sender's
  `0x1::account::Account` resource on the copy with the fork's control key, and creates that
  resource for an address that has none. On the copy, the account's own key no longer signs for
  it.
- When the sender holds less than 0.1 APT, chainsim gives it 1 APT for gas first and reports
  `gasTopUp`.
- The result has `hash`, `status` (`success` or `failure`), `vmStatus`, `gasUsed`,
  `gasUnitPrice` and `sequenceNumber`. A failed transaction exits 1 with `tx_reverted`, and the
  Move abort is in `data.vmStatus`.
- Every `send --as` and `set` is listed under `modifications` in `chainsim status`.

### set resource

`--bcs` is the whole new value of the resource as BCS hex. chainsim writes it into the session
and reads the resource back as JSON. Members of a resource group, such as object and fungible
store resources, are stored in their group and are refused with `readback_mismatch`.

### call

- `view` runs a Move view function: `'["0x1::coin::balance<0x1::aptos_coin::AptosCoin>", ["address:0x1"]]'`.
- `resource` reads a resource under an account: `'["0x1", "0x1::chain_id::ChainId"]'`.
- `ledger` returns the chain id, fork version, block height and timestamp.

There is no raw REST or JSON-RPC, and `--from` exits 3.

## Tokens

| Symbol | Metadata | Decimals |
| --- | --- | --- |
| USDC | `0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b` | 6 |

USDC is Circle's native fungible asset. Funding takes it from
`0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b`, the largest primary USDC
store in the Aptos indexer on 2026-11-01 (152.1 million USDC).

## Selftest

- `chain-id`: `0x1::chain_id::ChainId` reads 1 at the fork version.
- `fund`: the control key gets 1 APT with the CLI's store write, and it reads back.
- `transfer`: 0.1 APT from the control key to the throwaway receiver
  `0x000000000000000000000000000000000000000000000000000000000000c5e2` executes and moves the
  receiver's balance.
- `new-block`: a block moves the on-chain timestamp forward.
- `token-fund`: 1 USDC moves from the holder to the control key by impersonation and reads back.

Each run leaves its transfers in place on the copy.

## Limits

- Experimental beta engine, and the copy serves no REST API, so `rpcUrl` is `null`.
- Every transaction runs one at a time through the CLI.
- `send --as` replaces the authentication key of each account it sends as, on the copy.
- Signatures bind chain id 1, so a transaction signed for the copy is valid on Aptos Mainnet for
  that key and sequence number.
- Public rate limits apply without an API key, and the public fullnode keeps about the last
  150 million versions.
- Untouched state is read from the upstream on first access.
- The upstream URL and API key stay in the session file on the fork volume until `down`.
- The CLI is under the Innovation-Enabling Source Code License.
