# Architecture

## One engine per chain family

EVM chains run Foundry's anvil, and Arc runs Circle's arc-foundry build of it. Sui runs
chainsim's build of Mysten's `sui-fork`, with one patch (see [chains/sui](../chains/sui/README.md)). Each engine is a container.
No engine is linked into the binary: chainsim drives Docker through its API.

## Two containers per fork

`up` creates the network `chainsim-<name>`, the engine container, and a control sidecar
(`curlimages/curl`, or `fullstorydev/grpcurl` for Sui). chainsim runs every engine call as a
`docker exec` in that sidecar, so a fork needs no host port. Docker labels on the containers
are the fork registry: chainsim keeps no state file.

## Joining a caller's network

A test harness in containers can put the fork on its own network under a host name of its
choice:

```sh
docker network create qa-net
chainsim up ethereum-mainnet --name eth-qa --network qa-net --alias eth
docker run --rm --network qa-net curlimages/curl:8.16.0 -s -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://eth:8545
chainsim down eth-qa
docker network rm qa-net
```

## Reaching a fork from the host

`--publish` binds a free port on `127.0.0.1` only and reports it as `fork.hostRpcUrl`, for a
tool on the host such as `forge`, a script or a browser wallet:

```sh
chainsim up ethereum-mainnet --name eth-host --publish
chainsim down eth-host
```

## The engine lock

`engines.lock.json` pins every engine image by tag and digest per architecture. For an engine
chainsim builds, it pins the source instead, such as arc-foundry's release tarball and its
checksum. chainsim's own `sui-fork` follows the same pattern: our `engine` workflow compiles it from
the pinned commit and publishes the binary as a release asset, and the lock pins each tarball's
sha256 next to the commit, toolchain and base images it came from. `up` pulls or builds an
engine on first use and tags it `chainsim/<engine>:<version>`. `chainsim pull`, `chainsim engines` and `chainsim prune` manage
engines ahead of time:

```sh
chainsim pull foundry
chainsim engines
```

## Upstreams

Each chain reads mainnet state from a public RPC by default. For heavier runs, or to fork an
old block, set the chain's variable (for example `CHAINSIM_ETHEREUM_MAINNET_RPC`) to a provider
URL. chainsim passes it to the engine through the container environment and never writes it to
files, logs or output: errors scrub the host. `chainsim describe <chain>` shows the variable and
whether it is set.

## Profiles

Each chain is one file, `profiles/<chain>.json`, embedded in the binary: chain id, engine,
upstream, tokens, overrides, warm-up, selftest extras and limits. Adding an EVM chain takes a
new profile, not new code.
