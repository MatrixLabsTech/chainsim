// Benches for a chainsim fork of HyperEVM mainnet, with viem. bench/run.sh starts the forks and
// this script measures them. Results go to bench.json beside this script, and the page
// README.md is written from that file.
//   CHAINSIM_FORK  the fork's name (default hl-bench)
//   BENCH_KEY      a test key with no mainnet history
//   BENCH_MACHINE  the machine's name for the results (the container only sees Docker's VM)
//   BENCH_UPS      cold starts to time (default 5)
//   BENCH_ACCOUNTS mainnet accounts for the mirror bench, comma separated
//   CHAINSIM_COMMIT the commit chainsim was built from, for the page
//   node bench.mjs render    writes the page again from the JSON
import { execFile, execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { promisify } from 'node:util';
import { createPublicClient, createWalletClient, defineChain, encodeAbiParameters, http } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';

const OUT = new URL('./', import.meta.url);
const FORK = process.env.CHAINSIM_FORK ?? 'hl-bench';
const PROXY = `http://${FORK}:8545`;
const ENGINE = `http://${FORK}-engine:8545`;
const CORE_WRITER = '0x3333333333333333333333333333333333333333';
const PROVIDER = Boolean(process.env.CHAINSIM_HYPEREVM_MAINNET_RPC);
// Mainnet accounts of three sizes for the mirror bench: 5, about 20 and 177 positions.
const ACCOUNTS = (process.env.BENCH_ACCOUNTS || '0x2e30c32738a28833e3d85189c04895c25ee54cb5,0x645b2eeaa0a46df3c4211bebda1b2c7703e287b8,0x010461c14e146ac35fe42271bdc1134ee31c703a').split(',');
const LOCK = JSON.parse(readFileSync(new URL('../../../engines.lock.json', import.meta.url), 'utf8'));

const run = promisify(execFile);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => Number(process.hrtime.bigint()) / 1e6;
const sh = (cmd, ...args) => execFileSync(cmd, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
const chainsim = async (...args) => {
  const { stdout } = await run('chainsim', args, { encoding: 'utf8' }).catch((e) => e);
  const out = JSON.parse(stdout);
  if (!out.ok) throw new Error(`chainsim ${args.join(' ')}: ${out.error.code}`);
  return out.data;
};
const round = (v, d = 2) => Number(v.toFixed(d));
const stats = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return { n: s.length, p50: round(q(0.5)), p95: round(q(0.95)), p99: round(q(0.99)), max: round(s[s.length - 1]) };
};
const rpcBody = (id, method, params = []) => JSON.stringify({ jsonrpc: '2.0', id, method, params });
const post = async (url, body) => (await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body })).json();
const account = process.env.BENCH_KEY ? privateKeyToAccount(process.env.BENCH_KEY) : undefined; // not needed to render
const chain = defineChain({ id: 999, name: 'HyperEVM fork', nativeCurrency: { name: 'HYPE', symbol: 'HYPE', decimals: 18 }, rpcUrls: { default: { http: [PROXY] } } });
const pub = createPublicClient({ chain, transport: http(PROXY) });
const wallet = createWalletClient({ chain, account, transport: http(PROXY) });
const action = (id, types, values) => `0x01${id.toString(16).padStart(6, '0')}${encodeAbiParameters(types.map((type) => ({ type })), values).slice(2)}`;
const coreWriterAbi = [{ type: 'function', name: 'sendRawAction', stateMutability: 'nonpayable', inputs: [{ name: 'data', type: 'bytes' }], outputs: [] }];
const network = `chainsim-${FORK}`;
const join = () => { try { sh('docker', 'network', 'connect', network, os.hostname()); } catch {} };
const leave = () => { try { sh('docker', 'network', 'disconnect', network, os.hostname()); } catch {} };

/// The fork's containers as [name, role, image], by chainsim's labels.
const containers = () => sh('docker', 'ps', '--filter', `label=org.chainsim.fork=${FORK}`, '--format', '{{.Names}}\t{{.Label "org.chainsim.role"}}\t{{.Image}}').trim().split('\n').filter(Boolean).map((l) => l.split('\t'));

/// Peak memory of the fork's containers, sampled every two seconds while `work` runs.
async function withMemory(work) {
  const peak = {};
  const roles = new Map(containers().map(([name, role]) => [name, role]));
  let stop = false;
  const sampler = (async () => {
    while (!stop) {
      const { stdout } = await run('docker', ['stats', '--no-stream', '--format', '{{.Name}} {{.MemUsage}}']).catch(() => ({ stdout: '' }));
      for (const line of stdout.split('\n')) {
        const [name, used] = line.split(' ');
        if (!roles.has(name)) continue;
        const mib = parseFloat(used) * (used.includes('GiB') ? 1024 : used.includes('KiB') ? 1 / 1024 : 1);
        peak[roles.get(name)] = Math.max(peak[roles.get(name)] ?? 0, mib);
      }
      await sleep(2000);
    }
  })();
  const out = await work();
  stop = true;
  await sampler;
  return { out, peakMemoryMiB: Object.fromEntries(Object.entries(peak).map(([k, v]) => [k, round(v, 1)])) };
}

/// eth_blockNumber through the proxy every 50 ms until `work` ends: what another client waits.
async function whilePolling(work) {
  const waits = [];
  let stop = false;
  const poller = (async () => {
    while (!stop) {
      const t = now();
      await post(PROXY, rpcBody(1, 'eth_blockNumber'));
      const d = now() - t;
      waits.push(d);
      if (d < 50) await sleep(50 - d);
    }
  })();
  const t = now();
  const out = await work();
  const ms = now() - t;
  stop = true;
  await poller;
  return { out, ms: round(ms, 0), clientWaitMs: stats(waits) };
}

async function httpLatency(url, n, warmup = 100) {
  const out = [];
  for (let i = 0; i < n + warmup; i++) {
    const t = now();
    await post(url, rpcBody(i, 'eth_chainId'));
    if (i >= warmup) out.push(now() - t);
  }
  return stats(out);
}

async function wsLatency(url, n, warmup = 100) {
  const sock = new WebSocket(url.replace('http', 'ws'));
  await new Promise((ok, err) => { sock.onopen = ok; sock.onerror = err; });
  const out = [];
  for (let i = 0; i < n + warmup; i++) {
    const t = now();
    await new Promise((ok) => { sock.onmessage = ok; sock.send(rpcBody(i, 'eth_chainId')); });
    if (i >= warmup) out.push(now() - t);
  }
  sock.close();
  return stats(out);
}

/// Requests a second with `clients` clients, each sending the next request when its last returns.
async function throughput(clients, seconds, body) {
  const end = now() + seconds * 1000;
  const lat = [];
  await Promise.all(Array.from({ length: clients }, async () => {
    while (now() < end) {
      const t = now();
      await post(PROXY, body);
      lat.push(now() - t);
    }
  }));
  return { clients, requestsPerSecond: round(lat.length / seconds, 0), latencyMs: stats(lat) };
}

async function main() {
  const result = {
    date: new Date().toISOString(),
    chainsim: (await chainsim('--version')).help.trim(),
    commit: process.env.CHAINSIM_COMMIT || null,
    proxy: LOCK.engines['hyperevm-proxy'].version,
    upstream: PROVIDER ? 'provider' : 'public RPC',
    machine: { name: process.env.BENCH_MACHINE || null, dockerCpus: Number(sh('docker', 'info', '--format', '{{.NCPU}}')), dockerMemoryGiB: round(Number(sh('docker', 'info', '--format', '{{.MemTotal}}')) / 2 ** 30, 1), arch: os.arch() },
  };

  // 1. Cold starts: `up` from nothing, then the first transaction, five times.
  const ups = [];
  const runs = Number(process.env.BENCH_UPS ?? 5);
  for (let i = 0; i < runs; i++) {
    await chainsim('down', FORK).catch(() => {});
    // The public RPC allows 100 requests a minute and one `up` makes about that many.
    if (i > 0 && !PROVIDER) await sleep(65_000);
    const t = now();
    const fork = await chainsim('up', 'hyperevm-mainnet', '--name', FORK);
    const upMs = now() - t;
    join();
    await chainsim('fund', FORK, account.address, '--native', '10');
    const t2 = now();
    await pub.waitForTransactionReceipt({ hash: await wallet.sendTransaction({ to: '0x000000000000000000000000000000000000dEaD', value: 1n }), pollingInterval: 5 });
    ups.push({ run: i + 1, upMs: round(upMs, 0), firstTransactionMs: round(now() - t2, 0), forkBlock: fork.forkPoint.number });
    if (i < runs - 1) leave();
  }
  const upMs = stats(ups.map((u) => u.upMs));
  result.up = { runs: ups, p50Ms: upMs.p50, slowestMs: upMs.max, firstTransactionP50Ms: stats(ups.map((u) => u.firstTransactionMs)).p50 };
  const selftest = await chainsim('selftest', FORK);
  result.selftest = { passed: selftest.parts.filter((p) => p.ok).length, parts: selftest.parts.length };

  const measured = await withMemory(async () => {
    // 2. Proxy overhead: eth_chainId through the proxy and straight to the engine.
    const overhead = {};
    for (const [kind, f] of [['http', httpLatency], ['webSocket', wsLatency]]) {
      const [proxy, engine] = [await f(PROXY, 2000), await f(ENGINE, 2000)];
      overhead[kind] = { request: 'eth_chainId', proxyMs: proxy, engineMs: engine, overheadP50Ms: round(proxy.p50 - engine.p50) };
    }
    result.proxyOverhead = overhead;

    // 3. Requests a second: an eth_call of the BTC mark price precompile through the proxy.
    const markCall = rpcBody(1, 'eth_call', [{ to: '0x0000000000000000000000000000000000000806', data: encodeAbiParameters([{ type: 'uint32' }], [0]) }, 'latest']);
    result.throughput = { request: 'eth_call markPx (0x806)', seconds: 5, runs: [] };
    for (const clients of [1, 4, 16]) result.throughput.runs.push(await throughput(clients, 5, markCall));

    // 4. Order to fill: 200 CoreWriter IOC buys of 0.001 BTC, each timed from send to its receipt
    // and to the moment the position precompile reads the new size.
    await chainsim('set', FORK, 'hypercore', 'spot-balance', account.address, '0', '100000000000000');
    const sendAction = (payload, nonce) => wallet.writeContract({ address: CORE_WRITER, abi: coreWriterAbi, functionName: 'sendRawAction', args: [payload], gas: 200_000n, ...(nonce === undefined ? {} : { nonce }) });
    await pub.waitForTransactionReceipt({ hash: await sendAction(action(7, ['uint64', 'bool'], [500_000_000_000n, true])), pollingInterval: 5 });
    const book = await post(`${PROXY}/info`, JSON.stringify({ type: 'l2Book', coin: 'BTC' }));
    const limit = BigInt(Math.round(Number(book.levels[1][0].px) * 1.02)) * 10n ** 8n;
    const order = action(1, ['uint32', 'bool', 'uint64', 'uint64', 'bool', 'uint8', 'uint128'], [0, true, limit, 100_000n, false, 3, 0n]);
    const positionCall = rpcBody(1, 'eth_call', [{ to: '0x0000000000000000000000000000000000000800', data: encodeAbiParameters([{ type: 'address' }, { type: 'uint16' }], [account.address, 0]) }, 'latest']);
    const szi = async () => BigInt((await post(PROXY, positionCall)).result.slice(0, 66));
    const [receipts, fills] = [[], []];
    for (let i = 0; i < 200; i++) {
      const before = await szi();
      const t = now();
      await pub.waitForTransactionReceipt({ hash: await sendAction(order), pollingInterval: 2 });
      receipts.push(now() - t);
      while ((await szi()) === before) if (now() - t > 10_000) throw new Error(`order ${i} did not fill`);
      fills.push(now() - t);
    }
    result.orderToFill = { orders: 200, size: '0.001 BTC', receiptMs: stats(receipts), fillVisibleMs: stats(fills) };

    // 5. Sustained orders: 2,000 smaller ones, sent back to back without waiting, until the last
    // one is filled.
    const SUSTAINED = 2000;
    const small = action(1, ['uint32', 'bool', 'uint64', 'uint64', 'bool', 'uint8', 'uint128'], [0, true, limit, 20_000n, false, 3, 0n]);
    const start = await szi();
    let nonce = await pub.getTransactionCount({ address: account.address, blockTag: 'pending' });
    const t = now();
    let last;
    for (let i = 0; i < SUSTAINED; i++) last = await sendAction(small, nonce++);
    await pub.waitForTransactionReceipt({ hash: last, pollingInterval: 2 });
    while ((await szi()) !== start + BigInt(SUSTAINED) * 20n) if (now() - t > 120_000) throw new Error('sustained orders did not all fill');
    const seconds = (now() - t) / 1000;
    result.sustainedOrders = { orders: SUSTAINED, size: '0.0002 BTC', seconds: round(seconds), ordersPerSecond: round(SUSTAINED / seconds, 0) };
    // Sizes in the position precompile's units: 100 for 0.001 BTC and 20 for 0.0002 BTC.
    result.ordersFilled = { sent: 200 + SUSTAINED, filled: Number(start) / 100 + Number((await szi()) - start) / 20 };

    // 6. Mirror time against position count, each from the same state.
    result.mirror = [];
    for (const user of ACCOUNTS) {
      const snap = await chainsim('snapshot', FORK);
      const m = await whilePolling(() => chainsim('set', FORK, 'hypercore', 'mirror', user));
      result.mirror.push({ positions: m.out.positions, ms: m.ms, infoCalls: m.out.infoCalls, upstreamCalls: m.out.upstreamCalls, clientWaitMs: m.clientWaitMs });
      await chainsim('revert', FORK, snap.id);
    }
    result.mirror.sort((a, b) => a.positions - b.positions);

    // 7. Replay time against candle count: one, three and seven days of hourly BTC candles.
    result.replay = [];
    const day = (n) => new Date(Date.UTC(2026, 10, 1 + n)).toISOString().slice(0, 10);
    for (const days of [1, 3, 7]) {
      const snap = await chainsim('snapshot', FORK);
      const r = await whilePolling(() => chainsim('time', FORK, '--replay', 'BTC', '--from', day(0), '--to', day(days)));
      result.replay.push({ candles: r.out.candles, ms: r.ms, msPerCandle: round(r.ms / r.out.candles, 1), clientWaitMs: r.clientWaitMs });
      await chainsim('revert', FORK, snap.id);
    }
  });
  result.peakMemoryMiB = measured.peakMemoryMiB;

  // 8. Image sizes of the fork's three containers.
  result.imagesMB = {};
  for (const [name, role] of containers()) {
    const image = sh('docker', 'inspect', name, '--format', '{{.Image}}').trim();
    result.imagesMB[role] = round(Number(sh('docker', 'image', 'inspect', image, '--format', '{{.Size}}')) / 1e6, 1);
  }

  leave();
  await chainsim('down', FORK);
  writeFileSync(new URL('bench.json', OUT), `${JSON.stringify(result, null, 1)}\n`);
  render();
  console.log(JSON.stringify({ upP50Ms: result.up.p50Ms, fillP50Ms: result.orderToFill.fillVisibleMs.p50, ordersPerSecond: result.sustainedOrders.ordersPerSecond, filled: result.ordersFilled }));
}

function render() {
  const r = JSON.parse(readFileSync(new URL('bench.json', OUT), 'utf8'));
  const s = (ms) => (ms / 1000).toFixed(1);
  const pct = (x) => `${x.p50} / ${x.p95} / ${x.p99}`;
  const machine = [r.machine.name, `Docker with ${r.machine.dockerCpus} CPUs and ${r.machine.dockerMemoryGiB} GiB`, r.machine.arch].filter(Boolean).join(', ');
  const where = `${r.date.slice(0, 10)}, ${r.chainsim}${r.commit ? ` built from commit ${r.commit}` : ''}${r.proxy ? `, proxy ${r.proxy}` : ''}, HyperEVM mainnet through the ${r.upstream}, ${machine}`;
  const big = r.mirror.at(-1);
  const page = [
    '# Benches',
    '',
    `This run: ${where}.`,
    '',
    'Reproduce it from a clone of the repository, with Docker as the only requirement:',
    '',
    '```sh',
    'examples/hyperliquid/scripts/docker.sh bench/run.sh',
    '```',
    '',
    'Every number here comes from [bench.json](bench.json), which the run writes.',
    '',
    '## Start',
    '',
    `- \`up\`, a cold fork with HyperCore, over ${r.up.runs.length} runs: ${s(r.up.p50Ms)} s at the median, ${s(r.up.slowestMs)} s at the slowest (${r.up.runs.map((u) => s(u.upMs)).join(', ')}).`,
    `- First transaction after \`up\`: ${r.up.firstTransactionP50Ms} ms at the median.`,
    `- Selftest: ${r.selftest.passed} of ${r.selftest.parts} checks.`,
    '',
    '## Orders',
    '',
    `${r.orderToFill.orders} CoreWriter IOC buys of ${r.orderToFill.size}, each a signed transaction sent through the proxy from one viem client. ${r.ordersFilled.filled.toLocaleString('en-US')} of ${r.ordersFilled.sent.toLocaleString('en-US')} orders in this run filled.`,
    '',
    '| Milliseconds | p50 | p95 | p99 | max |',
    '| --- | --- | --- | --- | --- |',
    `| Send to receipt | ${r.orderToFill.receiptMs.p50} | ${r.orderToFill.receiptMs.p95} | ${r.orderToFill.receiptMs.p99} | ${r.orderToFill.receiptMs.max} |`,
    `| Send to the fill read back through the \`0x800\` precompile | ${r.orderToFill.fillVisibleMs.p50} | ${r.orderToFill.fillVisibleMs.p95} | ${r.orderToFill.fillVisibleMs.p99} | ${r.orderToFill.fillVisibleMs.max} |`,
    '',
    `Sustained: ${r.sustainedOrders.orders.toLocaleString('en-US')} orders of ${r.sustainedOrders.size} sent back to back were all filled in ${r.sustainedOrders.seconds} s, ${r.sustainedOrders.ordersPerSecond} orders a second.`,
    '',
    '## RPC proxy',
    '',
    `2,000 \`${r.proxyOverhead.http.request}\` requests each way. The overhead is the difference of the medians.`,
    '',
    '| Milliseconds, p50 / p95 / p99 | Through the proxy | Straight to the engine | Overhead at p50 |',
    '| --- | --- | --- | --- |',
    `| HTTP | ${pct(r.proxyOverhead.http.proxyMs)} | ${pct(r.proxyOverhead.http.engineMs)} | ${r.proxyOverhead.http.overheadP50Ms} |`,
    `| WebSocket | ${pct(r.proxyOverhead.webSocket.proxyMs)} | ${pct(r.proxyOverhead.webSocket.engineMs)} | ${r.proxyOverhead.webSocket.overheadP50Ms} |`,
    '',
    `Requests a second through the proxy, \`${r.throughput.request}\` for ${r.throughput.seconds} s:`,
    '',
    '| Clients | Requests a second | Latency ms, p50 / p95 / p99 |',
    '| --- | --- | --- |',
    ...r.throughput.runs.map((t) => `| ${t.clients} | ${t.requestsPerSecond.toLocaleString('en-US')} | ${pct(t.latencyMs)} |`),
    '',
    '## Mirror and replay',
    '',
    'Each run starts from the same snapshot. The last column is the longest another client waited for `eth_blockNumber` while the command ran, polling every 50 ms.',
    '',
    '| Mirror of a mainnet account | Seconds | Info API calls | Upstream calls | Longest client wait, ms |',
    '| --- | --- | --- | --- | --- |',
    ...r.mirror.map((m) => `| ${m.positions} positions | ${s(m.ms)} | ${m.infoCalls} | ${m.upstreamCalls} | ${Math.round(m.clientWaitMs.max)} |`),
    '',
    '| Replay of hourly BTC candles | Seconds | Milliseconds per candle | Longest client wait, ms |',
    '| --- | --- | --- | --- |',
    ...r.replay.map((x) => `| ${x.candles} candles | ${s(x.ms)} | ${x.msPerCandle} | ${Math.round(x.clientWaitMs.max)} |`),
    '',
    '## Footprint',
    '',
    `- Peak memory during the run: ${Object.entries(r.peakMemoryMiB).map(([role, v]) => `${role} ${v} MiB`).join(', ')}.`,
    `- Images: ${Object.entries(r.imagesMB).map(([role, v]) => `${role} ${v} MB`).join(', ')}.`,
    '',
  ];
  writeFileSync(new URL('README.md', OUT), page.join('\n'));
}

if (process.argv[2] === 'render') render();
else await main();
