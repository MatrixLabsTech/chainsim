#!/bin/bash
# The team's normal test run, with chainsim around it: fork HyperEVM mainnet, run the Foundry unit
# and fuzz tests and the viem e2e scripts against the fork, then remove it.
#   E2E_RESULTS=<file> also writes the runs' numbers there as JSON.
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
  # The first up of a HyperEVM fork answers with hostRpcUrl null, and status has the published URL.
  chainsim up "$FORK" --publish > /tmp/fork.json; CHAINSIM_RPC=$(chainsim status "$FORK" | jq -r '.data.hostRpcUrl // empty')
else
  chainsim up "$FORK" > /tmp/fork.json; docker network connect "chainsim-$FORK" "$(hostname)"; CHAINSIM_RPC=$(jq -r '.data.rpcUrl // empty' /tmp/fork.json)
fi
[ -n "$CHAINSIM_RPC" ] || { echo "test.sh: chainsim gave no RPC URL for the fork $FORK, so the tests cannot reach it" >&2; exit 1; }
export CHAINSIM_RPC
jq -c '{fork: .data.chain, block: .data.forkPoint.number, rpc: env.CHAINSIM_RPC}' /tmp/fork.json
chainsim selftest "$FORK" | jq -c '{selftest: .ok, parts: (.data.parts | length)}'

# Unit tests read HyperCore through the precompiles. Give their vault a 0.01 BTC long first.
chainsim set "$FORK" hypercore position "$VAULT" 0 1000 750000000 | jq -c '{seeded: .data.helper, user: .data.user}'
[ -d lib/forge-std ] || forge install foundry-rs/forge-std --no-git >/dev/null
forge test --fork-url "$CHAINSIM_RPC" | tee /tmp/forge.txt

# The e2e scripts send real transactions. HyperCore follows them through the proxy.
(cd e2e && yarn install --frozen-lockfile --silent && node stop-loss.e2e.mjs && node replay-day.e2e.mjs)

if [ -n "${E2E_RESULTS:-}" ]; then
  runs=$(grep -o 'runs: [0-9]*' /tmp/forge.txt | head -1 | cut -d' ' -f2)
  jq --argjson runs "${runs:-0}" --arg chainsim "$(chainsim --version | jq -r .data.help | tr -d '\n')" --arg date "$(date -u +%F)" \
    '. + {date: $date, chainsim: $chainsim, fuzzRuns: $runs}' "$E2E_RESULTS" > /tmp/results.json && cp /tmp/results.json "$E2E_RESULTS"
fi
