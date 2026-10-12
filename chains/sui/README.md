<p align="center"><img src="../../assets/chains/sui.svg" height="96" alt="Sui"></p>

# Sui

chainsim runs a private copy of Sui Mainnet in Docker as the chain `sui-mainnet`. This page covers
what differs from the EVM chains: the engine, how each command works on Sui, and what the copy
cannot do. The commands and their JSON output are the same as on every chain. The
[README](../../README.md) covers the shared workflow.

The limits listed here are a summary. The source of truth is the `limits` list in
[`profiles/sui-mainnet.json`](../../profiles/sui-mainnet.json), which `chainsim describe sui-mainnet`
and every fork object report word for word.

## What chainsim solves here

- Sui has no anvil. Mysten's experimental `sui-fork` forks Sui mainnet, but it serves gRPC
  only, prints no ready line, has no snapshot API and ships a separate image per architecture.
  chainsim picks the image for the host, waits on gRPC health, and implements snapshot and
  revert by copying the fork's data directory.
- Funding a test address on a fork needs SUI from somewhere. chainsim transfers it from a
  large mainnet holder as an unsigned transaction that only the fork accepts.
- Acting as a TreasuryCap or AdminCap holder means building an unsigned programmable
  transaction by hand and resolving every object reference. `send --as` takes a Move function
  and plain arguments, encodes them from the function's signature, and sponsors the gas.
- Some state no transaction can reach, such as a protocol config that only an upgrade changes.
  chainsim builds its Sui engine with one added call that writes an object's contents, and
  `set object` decodes the object's fields so you name a field and its new value.

## Maturity: experimental

`describe` and the fork object report `"maturity": "experimental"`. The engine is a build of
Mysten's `sui-fork`, which Mysten marks highly experimental, and the copy differs from Sui Mainnet in ways
the EVM forks do not:

- Transactions run one at a time, one per checkpoint, so runs are slow and cannot run in parallel.
- The copy serves Sui gRPC only. It serves no JSON-RPC and no GraphQL, so apps that read through
  GraphQL cannot run against it, and it has no CORS layer for a browser app.
- The Clock (`0x6`) does not move unless you advance it.
- There is no gas estimation and no dry run: every transaction names its gas budget, and a failed
  transaction still executes and uses gas.
- Sui signatures do not bind to a chain, so a transaction signed for the copy is also valid on
  Sui Mainnet. Use test keys that hold no mainnet assets.

## Engine

chainsim runs its own build of `sui-fork`: Mysten's release `mainnet-v1.81.1` plus one patch that
adds `ForkingService/SetObject`, which `set object` calls. `describe sui-mainnet` names the build
under `engine` and its repo and commit under `engineSource`.

- The source is the branch `chainsim/mainnet-v1.81.1` of
  [MatrixLabsTech/sui](https://github.com/MatrixLabsTech/sui), a fork of MystenLabs/sui. The branch
  holds Mysten's tag commit `e04cf9aa` and one commit on top. `engines.lock.json` pins that commit
  by its full sha, so a build never follows the branch.
- The first `up` builds the image `chainsim/sui-fork-chainsim:<version>` from
  `engines/sui-fork-chainsim/Dockerfile`, and `chainsim pull sui-fork-chainsim` builds it ahead of
  time. A build stage on the pinned `rust:1.96.1-bookworm` toolchain fetches the commit, checks
  its sha and compiles `sui-fork`. chainsim then copies the binary onto Mysten's `sui-tools` image
  for the host architecture, pinned by digest: `mainnet-v1.81.1` for amd64 and
  `mainnet-v1.81.1-arm64` for arm64.
- A cold `up` that built the image took 6.4 minutes on arm64, with 18 CPUs and 8 GB for Docker
  (measured on 2026-10-11). We have not timed amd64. Later forks reuse the image, and
  `chainsim engines` shows whether it is built.
- The engine runs as uid 65534 (nobody), not root. `up` creates the fork volume owned by that
  user, mounts it at `/data`, and runs a small supervisor loop around `sui-fork` so that snapshot
  and revert can stop and restart it.
- The engine and its control sidecar run with a read-only root filesystem, every Linux capability
  dropped and `no-new-privileges`. Neither publishes a port unless you pass `--publish`, which
  binds `127.0.0.1` only.

### Why chainsim keeps its own fork of Sui

Stock `sui-fork` changes state only by running transactions. That covers acting as any address,
balances, tokens and owners, but not a field that no transaction can reach. `SetObject` writes an
object's contents directly, and stock `sui-fork` has no such call. The patch stays small: one
commit, four files, about 90 hand-written lines plus the regenerated proto code. Moving it to a
new release is one cherry-pick.

### Moving to a new Sui release

1. In a clone of the fork, fetch Mysten's new tag and branch from it:
   `git fetch https://github.com/MystenLabs/sui.git tag mainnet-v<new>`, then
   `git checkout -b chainsim/mainnet-v<new> mainnet-v<new>`.
2. Cherry-pick the current chainsim commit onto it. If the ForkingService proto changed upstream,
   regenerate `crates/sui-fork/src/proto/generated` with the crate's `build-protos` script.
3. Push the new branch to MatrixLabsTech/sui.
4. In `engines.lock.json`, update the `sui-fork-chainsim` entry: `version` and the `image` tag
   (`mainnet-v<new>-` plus the first 12 digits of the commit), the `source` branch, commit,
   upstream tag and upstream commit, the `toolchain` (the Rust version in the tag's
   `rust-toolchain.toml`, pinned by digest), and both `bases` (the new `sui-tools` tags, pinned by
   digest).
5. Copy the new `forking_service.proto` into `crates/chainsim-sui/proto/` and update the first
   limit in `profiles/sui-mainnet.json`. `cargo test --workspace` fails until both name the
   lock's commit.
6. Rebuild with `chainsim pull sui-fork-chainsim`, then run the Sui acceptance suite:
   `cargo test -p chainsim-sui --test acceptance -- --ignored --test-threads=1`.

## Upstream

The copy reads Sui Mainnet state from a GraphQL service, not from a full node:

- The default is Mysten's public endpoint, `https://graphql.mainnet.sui.io/graphql`.
- Set `CHAINSIM_SUI_MAINNET_GRAPHQL` to a provider's GraphQL URL to use it instead. chainsim keeps
  the URL in the environment only, and removes it and its host from every message. The public
  endpoint rate-limits, so heavier runs should use a provider.
- An upstream failure exits 5, and `next` names `CHAINSIM_SUI_MAINNET_GRAPHQL`.
- Objects load from the upstream on first use, so the first read of each object is slower.
- With a provider URL, `sui-fork` treats the chain as unknown instead of mainnet (inferred from its
  source, untested), which may change protocol behavior.

## Control path

The host has no route to the fork network, so chainsim reaches the engine through a control
sidecar on that network: `fullstorydev/grpcurl:v1.9.3-alpine`, pinned by digest. Every call is
`grpcurl` run in the sidecar with a protobuf-JSON body, and every upstream GraphQL query is
`wget` run there too. No gRPC client is built into chainsim.

`up` returns once the engine answers `grpc.health.v1.Health/Check` with `SERVING`, since
`sui-fork` prints no ready line. A cold fork took 13 to 38 s in our runs on arm64.

## Start a fork

```sh
chainsim describe sui-mainnet
chainsim up sui-mainnet
chainsim selftest sui-mainnet
```

`up` forks from the upstream's latest checkpoint and prints the fork object, with
`"forkPoint": {"kind": "checkpoint", "number": ...}`. `--block <checkpoint>` forks from an older
checkpoint, but `fund`, `send` and `set` need the funder's coins, which load only for a recent
fork point (see Fund below).

`selftest` checks four parts:

- `grpc-health`: the engine reports `SERVING`.
- `status`: `ForkingService/GetStatus` reports the labeled fork checkpoint.
- `clock`: `LedgerService/GetObject` returns the Clock `0x6`.
- `funded-transfer`: 1 SUI from the funder to the fixed throwaway address `0x…c5e2` executes
  within 10 s. The transfer stays on the copy.

## Fund

```sh
chainsim fund sui-mainnet 0xc0ffee --native 25
```

On Sui, `fund` gives SUI only, by a transfer on the copy from the profile's funder,
`0x935029ca5219502a47ac9b69f556ccf6e2198b5e7815cf50f68846f723739cbd`. It is labeled Binance 1 on
CoinCarp's Sui rich list and held 27.59 million SUI in address-owned coins when we checked on
2026-10-09. The transfer carries no signature: `sui-fork` executes a transaction with an empty
signature list as its sender. Nothing is sent on Sui Mainnet.

- `fund` only raises a balance: an address that already holds more than the amount asked exits 3.
  `set balance` (below) can lower one.
- `fund` refuses an address that owns any object on Sui Mainnet (exit 7), so you fund only fresh
  test addresses.
- `fund --token` exits 3 on Sui. Use `set token` for tokens.

`fund` works only on a fork point within about an hour of the upstream head. `up` seeds the
funder's coins into the copy's owned-object index, and the public GraphQL endpoint keeps
owned-object listings for about the last hour (57 minutes, measured on 2026-10-10). On an older
`--block`, `fund` exits 3 and `next` says to fork again without `--block`. We found no public
endpoint with a longer window, and we have not tested provider endpoints with longer retention.

## Snapshot and revert

```sh
chainsim snapshot sui-mainnet
chainsim revert sui-mainnet <id>
```

`sui-fork` has no snapshot API. chainsim stops `sui-fork`, copies its data directory inside the
fork volume, and lets the supervisor start it again. Each takes about 1.5 s. As on every chain,
`revert` consumes its id and returns a fresh snapshot of the restored state.

## Send as any address

`send --as` runs a transaction as any address without its key, on the copy only. The funder pays
the gas as a sponsor, so the sender needs no SUI. On Sui, `--to` names a Move function, and its
arguments follow:

```sh
chainsim send sui-mainnet --as 0xcf5455078123a7f42f98f237301f6def64a077af7f6bdb8ef57a7f301367209c \
  --to '0x2::coin::mint_and_transfer<0x0000c7fa394e8abac7eb529e06bb2d90d11a775ee23db1b115a170f062b622f3::vault::VAULT>' \
  0xef6752ebbb43af90e8abc41f763aeb5f2fdd7981072e403b2c3ffba4e266edfa 1000 0xc0ffee
```

This mints 1000 units of a mainnet coin as the address that holds its TreasuryCap. The cap and its
owner were checked on 2026-10-11. If either moves on mainnet after that, this example needs new IDs.

- The target is `package::module::function`, with type arguments in `<...>` when the function
  has them.
- chainsim reads the function's parameter types from the fork
  (`MovePackageService/GetFunction`) and encodes each argument to match:
  - integers `u8` to `u256` as decimal or `0x` hex, `bool` as `true` or `false`, and `address` and
    `ID` as `0x` hex
  - `String` as text, `vector<u8>` as `0x` hex bytes or text, other vectors as a JSON array, and
    `Option` as `none` or the value
  - an object parameter takes the object ID. chainsim looks the object up on the fork and passes
    owned and immutable objects by version and digest, and shared objects by their initial shared
    version, mutable when the parameter is `&mut` or by value.
  - leave out the trailing `&mut TxContext`, which the runtime passes.
- Before sending, chainsim checks that the sender owns every owned object it passes. If not, it
  exits 2 with no gas spent, and `next` names the owner to use with `--as`.
- `--data` takes a raw programmable transaction as protobuf JSON,
  `{"inputs": [...], "commands": [...]}`, instead of a Move function. For an object input given
  by `objectId` alone, chainsim fills in its version, digest or initial shared version from the
  fork. `--to` is then only a label.
- The result holds the digest, `status`, `gasUsed` (net, computation, storage and rebate), and
  the objects the transaction `created`, `mutated` and `deleted`, without the funder's gas coin.
- A Move abort exits 1 with `tx_reverted`, and `data.error.abort` holds the package,
  module, function, instruction and abort code. Because there is no dry run on Sui, the failed
  transaction still executes: it uses gas and bumps the versions of the shared objects it touched.
- `--sig` and `--value` are EVM-only and exit 2 on Sui.

## Set

`set balance`, `set token` and `set owner` change state on the copy by sending transactions as the
right owner. `set object` writes an object's contents through the engine. Each command reads the
result back.

```sh
chainsim set sui-mainnet balance 0xc0ffee 12.5
chainsim set sui-mainnet token 0xc0ffee 0x0000c7fa394e8abac7eb529e06bb2d90d11a775ee23db1b115a170f062b622f3::vault::VAULT 5000
chainsim set sui-mainnet owner 0x89c1a321291d15ddae5a086c9abc533dff697fde3d89e0ca836c41af73e36a75 0xc0ffee
```

The last line moves Cetus's CLMM `AdminCap` (checked on 2026-10-11) to `0xc0ffee` on the copy.

- `set balance` sets SUI exactly. It raises a balance with a transfer from the funder, and lowers
  one by sending the excess back to the funder as the address.
- `set token` sets a coin balance. With no `--from`, it mints as the coin's TreasuryCap owner, when an
  address holds the cap. When the cap cannot be found or is not held by an address, as for USDC,
  it exits 3 and `next` says to name a holder with `--from <holder>`, which moves the amount out
  of that holder's coins. Lowering sends the excess to the funder.
- `set owner` transfers an object as its current owner. Only address-owned objects whose type has
  `store` can move this way. Shared, immutable and object-owned objects, and types with `key` but
  not `store`, exit 3 with a reason and a `next`: such objects change only through their own
  module, with `send --as`.
- `set storage` and `set code` have no meaning on Sui and exit 3.

### Balances of mainnet addresses

The copy's balance index counts only coins it has seen: the funder's seeded coins and anything
created or changed on the copy. An address that held coins on Sui Mainnet before the fork point,
and was not seeded at `up`, reads less than it holds. One such address read 0 SUI on the copy and
46.04 SUI on mainnet. Those coins still exist on the copy and can be spent by ID. `set balance` and
`set token` add a `warning` field when the address owns objects on Sui Mainnet.

### Set object

```sh
chainsim set sui-mainnet object 0xdaa46292632c3c4d8f31f23ea0f9b36a28ff3677e9684980e4438403a67a3d8f --field protocol_fee_rate=1234
```

This sets the protocol fee rate in Cetus's CLMM `GlobalConfig` (checked on 2026-10-11) on the
copy. `set object` replaces a Move object's contents through `ForkingService/SetObject`, so it
skips the object's module and its checks. The object keeps its type, id and owner, and gets a new
version in a new checkpoint.

- `--field <name>=<value>` changes one field, and repeats for more. A nested field takes a dotted
  path, such as `fee_tiers.contents.0.value.fee_rate`, where a number picks a vector element.
- chainsim reads the object's contents and the layout of every type in them from the fork
  (`MovePackageService/GetDatatype`), changes the named fields, and encodes the object again. A
  field takes its value as `send --as` takes an argument of the same type. A field whose type is
  another struct, such as a `VecMap`, exits 2, and the message lists its fields to set one by one.
- `--bcs <hex>` replaces the whole contents instead. The bytes must start with the object's own id
  and decode as its type, or `set` exits 2.
- The result lists the `changed` paths (or `"bcs"`), the new `version`, and every field read back
  from the fork, with integers from `u64` up as strings, IDs as addresses and strings as text.
- A coin exits 3, and `next` gives the `set token` command, because the copy's balance index
  changes only through transactions. A package exits 3: its code cannot change. The object's own
  `id` cannot change either.
- Execution sees the new contents. With `protocol_fee_rate` set to 1234, a following
  `update_protocol_fee_rate` call by a holder of the role reported the old rate as 1234. With
  `package_version` set to 999, Cetus's version check aborted the next admin call.

## Call

```sh
chainsim call sui-mainnet LedgerService/GetObject '{"objectId": "0x6"}'
chainsim call sui-mainnet ForkingService/AdvanceClock '{"durationMs": 60000}'
```

`call` runs one gRPC method on the engine. The method is `Service/Method`: services without a
package get `sui.rpc.v2`, or `sui.forking.v1alpha` for `ForkingService`, and a fully qualified name
works as is. The params are one protobuf-JSON request object. `ForkingService` has
`AdvanceClock`, `AdvanceCheckpoint`, `GetStatus` and chainsim's `SetObject`. `time` and `mine`
are not chainsim commands on Sui, so move time with `AdvanceClock`. `call --from` exits 3: use `send --as`.

## Status and clean up

```sh
chainsim status sui-mainnet
chainsim down sui-mainnet
```

`status` adds the head checkpoint and the `modifications` list: every `send --as` and `set` since
`up`, so a report can name each result that depends on impersonation or an override. `down`
removes the engine, the sidecar, the fork volume and the network chainsim made.
