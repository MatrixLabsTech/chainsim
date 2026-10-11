---
name: chainsim
description: Local mainnet forks of Ethereum, Arc, Robinhood Chain, Solana and Sui. Use when work needs a chain fork, a funded test wallet on mainnet state, or time and state control on a local chain.
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
   variable named by `upstream.env`. Done when `chainsim describe <chain>` exits 0.
2. **Learn the chain.** Run `chainsim describe <chain>`. Done when the `limits` that touch this
   task, and the `upstream` it reads from, are in your working notes or report.
3. **Start the fork.** Run `chainsim up <chain>`. For a tool in a container, join its network
   with `--network <net> --alias <host>`. For a tool on the host, add `--publish`. Done when you
   hold the `fork` object it prints.
4. **Prove it is fit.** Run `chainsim selftest <name>`. Done when it exits 0. On any other exit,
   follow `error.next`, and stop the task if it stays red.
5. **Fund a fresh test wallet.** Run `chainsim fund <name> <address> --native <amount>` and
   `--token <symbol>:<amount>` as the task needs. Done when `fund` exits 0, since it returns the
   read-back balances.
6. **Act as any account, or change contract state, when the task needs it.** To call a function
   only an admin, owner or other role holder may call, run `chainsim send <name> --as <address>
   --to <target> [<arg>...]` with the role holder's address: no key is needed. The target is a
   contract with `--sig "<fn(types)>"` on EVM, or `package::module::function<T...>` on Sui. To
   change state directly, run `chainsim set <name> balance|token|owner|storage|code ...`;
   `chainsim describe <chain>` lists what each chain supports. Done when the receipt `status` is
   1 or the read-back matches what you set.
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
[<params-json>]`, or start the fork with `--publish` and use `fork.hostRpcUrl`.

Copy `fork.limits` into the report word for word: they are how the fork differs from the real
chain. `fork.upstream` shows where it reads mainnet state and which variable takes a provider URL.
