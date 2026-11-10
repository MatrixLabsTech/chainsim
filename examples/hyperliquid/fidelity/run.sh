#!/bin/bash
# Forks HyperEVM mainnet, compares the fork with mainnet and removes the fork. It writes
# fidelity.json and README.md in this folder. Run it with scripts/docker.sh fidelity/run.sh,
# or on a host that has chainsim, Docker, curl, jq and Node.js 22 with yarn, with CHAINSIM_PUBLISH=1.
set -euo pipefail
cd "$(dirname "$0")"
export CHAINSIM_FORK=${CHAINSIM_FORK:-hl-fidelity}
export MAINNET_RPC=${CHAINSIM_HYPEREVM_MAINNET_RPC:-}

cleanup(){ [ -z "${CHAINSIM_PUBLISH:-}" ] && docker network disconnect "chainsim-$CHAINSIM_FORK" "$(hostname)" >/dev/null 2>&1; chainsim down "$CHAINSIM_FORK" | jq -c '{down: .ok}'; }
trap cleanup EXIT

# The info API's prices just before `up`, to bracket the prices the fork takes.
export CTX_BEFORE=/tmp/ctx-before.json BEFORE_UP_MS=$(node -e 'console.log(Date.now())')
curl -sS -X POST -H 'content-type: application/json' https://api.hyperliquid.xyz/info --data '{"type":"metaAndAssetCtxs"}' > "$CTX_BEFORE"

if [ -n "${CHAINSIM_PUBLISH:-}" ]; then
  chainsim up hyperevm-mainnet --name "$CHAINSIM_FORK" --publish > /tmp/fork.json; export CHAINSIM_RPC=$(jq -r .data.hostRpcUrl /tmp/fork.json)
else
  chainsim up hyperevm-mainnet --name "$CHAINSIM_FORK" > /tmp/fork.json; docker network connect "chainsim-$CHAINSIM_FORK" "$(hostname)"; export CHAINSIM_RPC=$(jq -r .data.rpcUrl /tmp/fork.json)
fi
jq -c '{fork: .data.chain, block: .data.forkPoint.number}' /tmp/fork.json
yarn install --frozen-lockfile --silent
node fidelity.mjs
