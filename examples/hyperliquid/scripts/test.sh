#!/bin/bash
# The team's normal test run, with chainsim around it: fork HyperEVM mainnet, run the Foundry unit
# tests and the viem e2e script against the fork, then remove it.
#   In a container with the Docker socket: the script joins the fork's network.
#   On a host or CI runner: CHAINSIM_PUBLISH=1 publishes the proxy on 127.0.0.1.
set -euo pipefail
cd "$(dirname "$0")/.."
FORK=${CHAINSIM_FORK:-hyperevm-mainnet}
export E2E_KEY=${E2E_KEY:-$(cast keccak "chainsim-hl-example-test-key")}   # test key with no mainnet history
VAULT=0x0000000000000000000000000000000000005A1E                          # the unit tests' vault

cleanup(){ [ -z "${CHAINSIM_PUBLISH:-}" ] && docker network disconnect "chainsim-$FORK" "$(hostname)" >/dev/null 2>&1; chainsim down "$FORK" | jq -c '{down: .ok}'; }
trap cleanup EXIT

if [ -n "${CHAINSIM_PUBLISH:-}" ]; then
  chainsim up "$FORK" --publish > /tmp/fork.json; export CHAINSIM_RPC=$(jq -r .data.hostRpcUrl /tmp/fork.json)
else
  chainsim up "$FORK" > /tmp/fork.json; docker network connect "chainsim-$FORK" "$(hostname)"; export CHAINSIM_RPC=$(jq -r .data.rpcUrl /tmp/fork.json)
fi
jq -c '{fork: .data.chain, block: .data.forkPoint.number, rpc: env.CHAINSIM_RPC}' /tmp/fork.json
chainsim selftest "$FORK" | jq -c '{selftest: .ok, parts: (.data.parts | length)}'

# Unit tests read HyperCore through the precompiles. Give their vault a 0.01 BTC long first.
chainsim set "$FORK" hypercore position "$VAULT" 0 1000 750000000 | jq -c '{seeded: .data.helper, user: .data.user}'
forge test --fork-url "$CHAINSIM_RPC"

# The e2e script sends real transactions. HyperCore follows them through the proxy.
(cd e2e && yarn install --frozen-lockfile --silent && node stop-loss.e2e.mjs)
