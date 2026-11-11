#!/bin/bash
# Forks HyperEVM mainnet, compares the fork with mainnet and removes the fork. It writes
# fidelity.json and README.md in this folder. Run it with scripts/docker.sh fidelity/run.sh,
# or on a host that has chainsim, Docker, curl, jq and Node.js 22 with yarn, with CHAINSIM_PUBLISH=1.
set -euo pipefail
cd "$(dirname "$0")"
export CHAINSIM_FORK=${CHAINSIM_FORK:-hl-fidelity}
export MAINNET_RPC=${CHAINSIM_HYPEREVM_MAINNET_RPC:-}

cleanup(){ kill "${POLL:-}" 2>/dev/null || true; [ -z "${CHAINSIM_PUBLISH:-}" ] && docker network disconnect "chainsim-$CHAINSIM_FORK" "$(hostname)" >/dev/null 2>&1; chainsim down "$CHAINSIM_FORK" | jq -c '{down: .ok}'; }
trap cleanup EXIT

# The info API's prices from before `up` until it is done, read every 250 ms, to hold the range
# the prices the fork takes lie in.
yarn install --frozen-lockfile --silent
export CTX_READS=/tmp/ctx-reads.jsonl; : > "$CTX_READS"
node fidelity.mjs poll "$CTX_READS" & POLL=$!
for _ in $(seq 1 100); do [ -s "$CTX_READS" ] && break; sleep 0.1; done
[ -s "$CTX_READS" ] || { echo "run.sh: no price read from the info API" >&2; exit 1; }
export BEFORE_UP_MS=$(node -e 'console.log(Date.now())')

if [ -n "${CHAINSIM_PUBLISH:-}" ]; then
  chainsim up hyperevm-mainnet --name "$CHAINSIM_FORK" --publish > /tmp/fork.json; export CHAINSIM_RPC=$(jq -r .data.hostRpcUrl /tmp/fork.json)
else
  chainsim up hyperevm-mainnet --name "$CHAINSIM_FORK" > /tmp/fork.json; docker network connect "chainsim-$CHAINSIM_FORK" "$(hostname)"; export CHAINSIM_RPC=$(jq -r .data.rpcUrl /tmp/fork.json)
fi
kill "$POLL"
jq -c '{fork: .data.chain, block: .data.forkPoint.number}' /tmp/fork.json
node fidelity.mjs
