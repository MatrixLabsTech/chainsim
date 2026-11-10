---
name: chainsim
description: Local mainnet forks of Ethereum, Arc, Robinhood Chain, HyperEVM, Sui and Aptos. Use when work needs a chain fork, a funded test wallet on mainnet state, or time and state control on a local chain.
---

# chainsim

Every `chainsim` command prints one JSON object: `{"ok": true, "data": ...}` or
`{"ok": false, "error": {"code", "message", "next"}}`. Branch on the exit code, and when a
command fails, run what `error.next` says. `chainsim describe` and `chainsim --help` hold every
fact about chains, operations, limits and upstreams, so read them instead of assuming.

Work through these steps in order. Each ends on a check you can make.

1. **Check the setup.** If `chainsim` is not found, ask the user before installing it with
   `curl -fsSL https://raw.githubusercontent.com/MatrixLabsTech/chainsim/main/install.sh | sh`,
   and put the folder it names on `PATH`. Then run `chainsim doctor`. If it exits 1 with
   `docker_unavailable`, follow `error.next`. In Codex, the default sandbox blocks the Docker
   socket, so run chainsim outside it by asking for the escalation. Find the chain under
   `data.chains`: when `upstream.set` is false the fork reads the public `upstream.default`, so
   check the chain's `limits` and tell the user when the task needs a provider URL in the
   variable named by `upstream.env`. On Aptos, check that `CHAINSIM_APTOS_API_KEY` is set: without
   it the public endpoint's per-IP limit stops a run after a few operations, so ask the user for a
   key from https://geomi.dev. Done when `chainsim describe <chain>` exits 0.
2. **Learn the chain.** Run `chainsim chains --json` to see which chains exist and which features
   each supports, then `chainsim describe <chain>`. Done when the `limits` that touch this
   task, and the `upstream` it reads from, are in your working notes or report.
3. **Start the fork.** Run `chainsim up <chain>`. For a tool in a container, join its network
   with `--network <net> --alias <host>`. For a tool on the host, add `--publish`. An Aptos fork
   serves no RPC, so it takes neither and its `rpcUrl` is `null`. Done when you hold the `fork`
   object it prints.
4. **Prove it is fit.** Run `chainsim selftest <name>`. Done when it exits 0. On any other exit,
   follow `error.next`, and stop the task if it stays red.
5. **Fund a fresh test wallet.** Run `chainsim fund <name> <address> --native <amount>` and
   `--token <symbol>:<amount>` as the task needs. Done when `fund` exits 0, since it returns the
   read-back balances.
6. **Act as any account, or change contract state, when the task needs it.** To call a function
   only an admin, owner or other role holder may call, run `chainsim send <name> --as <address>
   --to <target> [<arg>...]` with the role holder's address: no key is needed. The target is a
   contract with `--sig "<fn(types)>"` on EVM, `package::module::function<T...>` on Sui, or
   `address::module::function<T...>` with `<type>:<value>` arguments on Aptos. To change state
   directly, run `chainsim set <name> balance|token|owner|object|resource|storage|code ...`. On
   HyperEVM, `set <name> hypercore price|spot-balance|position ...` and `set <name> precompile`
   set what contracts read from HyperCore, `set <name> hypercore mirror <user>` copies a mainnet
   account's Core state onto the copy at mainnet's prices, `call <name> hypercore_state <user>` reads an account's
   Core state, and `call <name> corewriter_actions` lists the CoreWriter actions contracts sent,
   decoded, with what each did. `set <name> hypercore price` moves a perp (fills and liquidations
   follow), `set <name> hypercore funding` sets a funding rate, and `time <name> --replay <perp>
   --from <date> --to <date>` replays mainnet candles. Builder-deployed perps are named `dex:coin`
   (`set hypercore book xyz:XYZ100 --live`), and `hyperevm-testnet` works the same way. Core actions apply after every block, whoever sends them, and the fork answers the Hyperliquid
   info API at `<rpcUrl>/info`.
   `chainsim describe <chain>` lists what each chain supports. Done when the receipt `status` is
   1 (`success` on Aptos) or the read-back matches what you set.
7. **Do the work.** Before each scenario run `chainsim snapshot <name>`, and after it
   `chainsim revert <name> <id>`. Done when every scenario has a pass or fail and each `revert`
   exited 0.
8. **Report and clean up.** Run `chainsim status <name>` and list each entry under
   `modifications` in the report, since results that depend on them would not happen on mainnet
   as they did on the fork. Then run `chainsim down <name>`. Done when the report lists them and
   `down` exits 0.

## The fork object

`fork` is what `up` returns and every later step reads. Its `rpcUrl` is in-network: containers on
`fork.network` reach it, the host does not. From the host, use `chainsim call <name> <method>
[<params-json>]`, or start the fork with `--publish` and use `fork.hostRpcUrl`. On Aptos,
`rpcUrl` is `null`: read the fork with `chainsim call <name> view|resource|ledger`. On HyperEVM,
`fork.hypercore` lists the HyperCore answers seeded at `up`.

Copy `fork.limits` into the report word for word: they are how the fork differs from the real
chain. `fork.upstream` shows where it reads mainnet state and which variable takes a provider URL.
