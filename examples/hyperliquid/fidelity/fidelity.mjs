// Fidelity check: a chainsim fork of HyperEVM mainnet against mainnet itself.
// It reads the same HyperCore precompiles on both, mirrors two mainnet accounts and compares
// their state with the info API, checks fills against the order book and one hour of funding
// against rate times notional. Results go to fidelity.json beside this script, and the
// page README.md is written from that file.
//   CHAINSIM_RPC   the fork's rpcUrl
//   CHAINSIM_FORK  the fork's name
//   MAINNET_RPC    HyperEVM mainnet (default https://rpc.hyperliquid.xyz/evm)
//   FIDELITY_VAULT, FIDELITY_ACCOUNT   the two mainnet accounts to mirror
//   CHAINSIM_COMMIT the commit chainsim was built from, for the page
//   CTX_BEFORE     a file with the info API's metaAndAssetCtxs from just before `up`
//   BEFORE_UP_MS   when that file was read, in ms
//   node fidelity.mjs render    writes the page again from the JSON
import { execFile, execFileSync } from 'node:child_process';
import { promisify } from 'node:util';
import { readFileSync, writeFileSync } from 'node:fs';
import { decodeAbiParameters, encodeAbiParameters, parseUnits } from 'viem';

const OUT = new URL('./', import.meta.url);
const RPC = process.env.CHAINSIM_RPC;
const FORK = process.env.CHAINSIM_FORK ?? 'hl-fidelity';
const MAINNET = process.env.MAINNET_RPC || 'https://rpc.hyperliquid.xyz/evm';
const INFO = 'https://api.hyperliquid.xyz/info';
const VAULT = process.env.FIDELITY_VAULT || '0x010461c14e146ac35fe42271bdc1134ee31c703a'; // an HLP strategy vault
const TRADER = process.env.FIDELITY_ACCOUNT || '0x7a87ac0f820afeb0e1cdb846ef1ab068d21bf740'; // a mainnet account on standard margin, with no resting orders
const WALLET = '0x00000000000000000000000000000000f1de1147'; // exists only on the fork
const CORE_WRITER = '0x3333333333333333333333333333333333333333';
const TAKER_FEE = 0.00045;

const LOCK = JSON.parse(readFileSync(new URL('../../../engines.lock.json', import.meta.url), 'utf8'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const post = async (url, body) => {
  for (let i = 0; ; i++) {
    const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
    if (r.ok) return r.json();
    if (i === 5) throw new Error(`${new URL(url).host}: HTTP ${r.status}`);
    await sleep(3000 * (i + 1));
  }
};
const mainInfo = (body) => post(INFO, body);
const forkInfo = (body) => post(`${RPC}/info`, body);
const chainsim = (...args) => {
  const out = JSON.parse(execFileSync('chainsim', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }));
  if (!out.ok) throw new Error(`chainsim ${args.join(' ')}: ${out.error.code}`);
  return out.data;
};
const chainsimAsync = async (...args) => {
  const out = JSON.parse((await promisify(execFile)('chainsim', args, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })).stdout);
  if (!out.ok) throw new Error(`chainsim ${args.join(' ')}: ${out.error.code}`);
  return out.data;
};
const pc = (n) => `0x${n.toString(16).padStart(40, '0')}`;
const u32 = (v) => encodeAbiParameters([{ type: 'uint32' }], [v]);
/// eth_call for each entry in JSON-RPC batches, answers in order. The public mainnet RPC counts
/// every call of a batch against its 100 requests a minute, so a refused batch is sent again.
const calls = async (url, list) => {
  const answers = [];
  for (let at = 0; at < list.length; at += 20) {
    const part = list.slice(at, at + 20);
    for (let tries = 0; ; tries++) {
      const out = await post(url, part.map((c, id) => ({ jsonrpc: '2.0', id, method: 'eth_call', params: [{ to: c.to, data: c.data }, 'latest'] })));
      if (Array.isArray(out) && out.every((o) => o.result !== undefined)) {
        const byId = new Map(out.map((o) => [o.id, o.result]));
        answers.push(...part.map((_, id) => byId.get(id)));
        break;
      }
      if (tries === 10) throw new Error(`${new URL(url).host}: ${JSON.stringify(out).slice(0, 200)}`);
      await sleep(15_000);
    }
  }
  return answers;
};
const bps = (a, b) => (Number(b) === 0 ? (Number(a) === 0 ? 0 : Infinity) : (Math.abs(Number(a) - Number(b)) / Math.abs(Number(b))) * 1e4);
const round = (v, d = 4) => (Number.isFinite(v) ? Number(v.toFixed(d)) : null);
/// A group's summary: how many items are equal and the largest difference among them.
const group = (name, rule, items) => ({
  name,
  rule,
  equal: items.filter((i) => i.equal).length,
  total: items.length,
  largestDiffBps: round(Math.max(0, ...items.map((i) => i.diffBps ?? 0))),
  items,
});

async function run() {
  const result = { date: new Date().toISOString(), chainsim: chainsim('--version').help.trim(), upstream: process.env.MAINNET_RPC ? 'provider' : 'public RPC', commit: process.env.CHAINSIM_COMMIT || null, proxy: LOCK.engines['hyperevm-proxy'].version, groups: [] };
  const status = chainsim('status', FORK);
  result.forkBlock = status.forkPoint.number;
  // The info API's prices right after `up`. run.sh saved them just before it.
  const afterUpMs = Date.now();
  const ctxAfter = await mainInfo({ type: 'metaAndAssetCtxs' });
  const ctxBefore = JSON.parse(readFileSync(process.env.CTX_BEFORE, 'utf8'));
  const meta = await mainInfo({ type: 'meta' });
  const perpIndex = new Map(meta.universe.map((u, i) => [u.name, i]));
  const szDecimals = new Map(meta.universe.map((u) => [u.name, u.szDecimals]));
  const seeded = ['BTC', 'ETH', 'SOL', 'HYPE'].map((coin) => ({ coin, perp: perpIndex.get(coin) }));

  // 1. Prices, before any mirror moves them. The fork holds the prices `up` took, so they lie
  // between the info API's just before and just after `up`.
  const perps = seeded.flatMap((s) => [{ what: `markPx ${s.coin} (0x806)`, to: pc(0x806), data: u32(s.perp), coin: s.coin, key: 'markPx' }, { what: `oraclePx ${s.coin} (0x807)`, to: pc(0x807), data: u32(s.perp), coin: s.coin, key: 'oraclePx' }]);
  const forkPx = await calls(RPC, perps);
  const ctxPx = (ctxs, p) => Number(ctxs[1][perpIndex.get(p.coin)][p.key]);
  result.groups.push({
    ...group('Price precompile reads', 'between the info API price read just before up and the one read just after, or within 1 bp of one of them', perps.map((p, i) => {
      const x = Number(BigInt(forkPx[i])) / 10 ** (6 - szDecimals.get(p.coin));
      const [lo, hi] = [ctxPx(ctxBefore, p), ctxPx(ctxAfter, p)].sort((m, n) => m - n);
      const d = x >= lo && x <= hi ? 0 : Math.min(bps(x, lo), bps(x, hi));
      return { what: p.what, fork: x, mainnetBeforeUp: ctxPx(ctxBefore, p), mainnetAfterUp: ctxPx(ctxAfter, p), equal: d <= 1, diffBps: round(d) };
    })),
    secondsOfUp: round((afterUpMs - Number(process.env.BEFORE_UP_MS ?? afterUpMs)) / 1000, 1),
  });
  // 2. Mirrors: the info API before and after, the fork between. A mirror brings the account's
  // perps to mainnet's prices, so the second account is mirrored two minutes after `up`, when
  // the prices `up` took are old.
  const positionReads = [];
  for (const [label, user] of [['HLP strategy vault', VAULT], ['Account on standard margin', TRADER]]) {
    if (user === TRADER) await sleep(Math.max(0, afterUpMs + 125_000 - Date.now()));
    if (user === TRADER) result.secondMirrorSecondsAfterUp = round((Date.now() - afterUpMs) / 1000, 1);
    // Mainnet keeps trading and repricing. The info API is read before the mirror, every quarter
    // second while it runs and after it, and the fork must hold what one of those reads gave.
    const state = () => mainInfo({ type: 'clearinghouseState', user });
    const reads = [await state()];
    const t0 = Date.now();
    let running = true;
    const polling = (async () => {
      while (running) {
        await sleep(250);
        const r = await state().catch(() => null);
        if (r) reads.push(r);
      }
    })();
    const mirrored = await chainsimAsync('set', FORK, 'hypercore', 'mirror', user);
    running = false;
    await polling;
    reads.push(await state());
    const windowMs = Date.now() - t0;
    const fork = await forkInfo({ type: 'clearinghouseState', user });
    const openOrders = (await mainInfo({ type: 'openOrders', user })).length;
    const byCoin = (s) => new Map(s.assetPositions.map((p) => [p.position.coin, p.position]));
    const [f, all] = [byCoin(fork), reads.map(byCoin)];
    const coins = [...f.keys()].filter((c) => all.every((m) => m.has(c)));
    const seen = (c, get) => all.map((m) => get(m.get(c)));
    const nearest = (x, values) => values.reduce((m, v) => (Math.abs(v - x) < Math.abs(m - x) ? v : m));
    const SAME = 'equal to a value the info API gave for the position while the mirror ran';
    const same = (field, get) => group(`${label}: position ${field}`, SAME, coins.map((c) => ({ what: c, equal: seen(c, get).includes(get(f.get(c))) })));
    // USD figures are compared to six decimals, the precision the info API prints them at.
    const usd = (field) => group(`${label}: position ${field}`, `${SAME}, to six decimals`, coins.map((c) => {
      const x = Number(f.get(c)[field]);
      const m = nearest(x, seen(c, (p) => Number(p[field])));
      return { what: c, fork: f.get(c)[field], mainnet: String(m), equal: Math.round(Math.abs(m - x) * 1e6) <= 1, diffBps: round(bps(x, m)) };
    }));
    // Entry notional, the position value less the unrealized PnL, does not depend on the mark.
    const entryNtl = (p) => Number(p.positionValue) - Math.sign(Number(p.szi)) * Number(p.unrealizedPnl);
    const entry = group(`${label}: position entry notional`, 'position value less unrealized PnL, within 0.01 USD of a value the info API gave while the mirror ran', coins.map((c) => {
      const x = entryNtl(f.get(c));
      const m = nearest(x, seen(c, entryNtl));
      return { what: c, fork: round(x, 6), mainnet: round(m, 6), equal: Math.abs(m - x) <= 0.01, diffBps: round(bps(x, m)) };
    }));
    // A figure that depends on the whole account moves with every mark: the fork's lies within
    // the range the reads span, or within 1 bp of one of them.
    const ranged = (x, values) => {
      const [lo, hi] = [Math.min(...values), Math.max(...values)];
      return { lo, hi, d: x >= lo && x <= hi ? 0 : bps(x, nearest(x, values)) };
    };
    const set = coins.filter((c) => seen(c, (p) => p.liquidationPx).every((v) => v !== null));
    const liquidation = group(`${label}: position liquidationPx`, 'within the range the info API gave while the mirror ran, or within 1 bp of it', set.map((c) => {
      const r = f.get(c).liquidationPx === null ? { lo: null, hi: null, d: Infinity } : ranged(Number(f.get(c).liquidationPx), seen(c, (p) => Number(p.liquidationPx)));
      return { what: c, fork: f.get(c).liquidationPx, mainnetLow: r.lo, mainnetHigh: r.hi, equal: r.d <= 1, diffBps: round(r.d) };
    }));
    const unset = coins.filter((c) => seen(c, (p) => p.liquidationPx).every((v) => v === null));
    const noLiquidation = group(`${label}: positions with no liquidation price`, 'null on the fork where the info API gives null', unset.map((c) => ({ what: c, equal: f.get(c).liquidationPx === null })));
    const total = (path, get) => {
      const r = ranged(Number(get(fork)), reads.map((s) => Number(get(s))));
      return { what: path, fork: get(fork), mainnetLow: r.lo, mainnetHigh: r.hi, equal: r.d <= 1, diffBps: round(r.d) };
    };
    const totals = [
      total('marginSummary.accountValue', (s) => s.marginSummary.accountValue),
      total('marginSummary.totalNtlPos', (s) => s.marginSummary.totalNtlPos),
      total('marginSummary.totalRawUsd', (s) => s.marginSummary.totalRawUsd),
      total('marginSummary.totalMarginUsed', (s) => s.marginSummary.totalMarginUsed),
      total('crossMaintenanceMarginUsed', (s) => s.crossMaintenanceMarginUsed),
      total('withdrawable', (s) => s.withdrawable),
    ];
    result.groups.push(
      { ...group(`${label}: positions mirrored`, 'every position the info API lists is on the fork', [{ what: 'count', fork: f.size, mainnet: all[all.length - 1].size, equal: all.some((m) => m.size === f.size) }]), positions: mirrored.positions, pricesMoved: mirrored.marks.length, windowMs, mainnetReads: reads.length, openOrders },
      same('szi', (p) => Number(p.szi)), same('entryPx', (p) => Number(p.entryPx)), same('leverage', (p) => JSON.stringify(p.leverage)), same('maxLeverage', (p) => p.maxLeverage),
      same('cumFunding', (p) => ['allTime', 'sinceOpen', 'sinceChange'].map((k) => Number(p.cumFunding[k]).toFixed(5)).join()),
      usd('positionValue'), usd('marginUsed'), usd('unrealizedPnl'), entry, liquidation, noLiquidation,
      group(`${label}: account summary`, 'within the range the info API gave while the mirror ran, or within 1 bp of it', totals),
    );
    // The position precompile on both sides, for the 20 largest positions (read further down).
    const last = all[all.length - 1];
    const largest = coins.filter((c) => perpIndex.has(c)).sort((x, y) => Number(last.get(y).positionValue) - Number(last.get(x).positionValue)).slice(0, 20);
    positionReads.push({ label, user, all, reads: largest.map((c) => ({ what: c, to: pc(0x800), data: encodeAbiParameters([{ type: 'address' }, { type: 'uint16' }], [user, perpIndex.get(c)]) })) });
  }

  // 3. Static precompile answers, byte for byte.
  const statics = [
    ...seeded.map((s) => ({ what: `perpAssetInfo ${s.coin} (0x80a)`, to: pc(0x80a), data: u32(s.perp) })),
    { what: 'tokenInfo USDC (0x80c)', to: pc(0x80c), data: u32(0) },
    { what: 'tokenInfo HYPE (0x80c)', to: pc(0x80c), data: u32(150) },
    { what: 'spotInfo HYPE/USDC (0x80b)', to: pc(0x80b), data: u32(107) },
  ];
  const [sf, sm] = [await calls(RPC, statics), await calls(MAINNET, statics)];
  result.groups.push(group('Static precompile reads', 'the same eth_call on the fork and on mainnet returns the same bytes', statics.map((s, i) => ({ what: s.what, equal: sf[i] !== null && sf[i] === sm[i], bytes: sf[i] ? (sf[i].length - 2) / 2 : 0 }))));

  // 4. The position precompile on both sides for the mirrored accounts. A position mainnet traded
  // while the mirror ran or since (its precompile size is not the one every info API read gave) is left out.
  for (const { label, all, reads } of positionReads) {
    const [rm, rf] = [await calls(MAINNET, reads), await calls(RPC, reads)];
    const pos = (hex) => decodeAbiParameters([{ type: 'int64' }, { type: 'uint64' }, { type: 'int64' }, { type: 'uint32' }, { type: 'bool' }], hex);
    const raw = (m, c) => parseUnits(Number(m.get(c).szi).toFixed(szDecimals.get(c)), szDecimals.get(c));
    const items = reads.map((r, i) => ({ r, x: pos(rf[i]), y: pos(rm[i]) })).filter(({ r, y }) => all.every((m) => y[0] === raw(m, r.what)));
    result.groups.push(group(`${label}: position precompile (0x800)`, 'size, leverage and margin mode equal on the fork and on mainnet, for the 20 largest positions, less those mainnet traded while the mirror ran or since', items.map(({ r, x, y }) => ({ what: r.what, fork: `${x[0]} ${x[3]}x ${x[4] ? 'isolated' : 'cross'}`, mainnet: `${y[0]} ${y[3]}x ${y[4] ? 'isolated' : 'cross'}`, equal: x[0] === y[0] && x[3] === y[3] && x[4] === y[4] }))));
  }

  // 5. Fills: IOC buys at three sizes against the average price the book gives.
  chainsim('fund', FORK, WALLET, '--native', '1');
  chainsim('set', FORK, 'hypercore', 'spot-balance', WALLET, '0', '1000000000000000');
  const action = (id, types, values) => `0x01${id.toString(16).padStart(6, '0')}${encodeAbiParameters(types.map((type) => ({ type })), values).slice(2)}`;
  const send = (payload) => chainsim('send', FORK, '--as', WALLET, '--to', CORE_WRITER, '--sig', 'sendRawAction(bytes)', payload);
  send(action(7, ['uint64', 'bool'], [5_000_000_000_000n, true]));
  const fills = [];
  let seen = 0;
  for (const want of ['small', 'medium', 'past the best level']) {
    const book = await forkInfo({ type: 'l2Book', coin: 'BTC' });
    const asks = book.levels[1].map((l) => ({ px: parseUnits(l.px, 8), sz: parseUnits(l.sz, 8) }));
    const sz = want === 'small' ? parseUnits('0.001', 8) : want === 'medium' ? parseUnits('0.01', 8) : asks[0].sz + asks[1].sz + asks[2].sz / 2n;
    const size = (sz / 1000n) * 1000n; // BTC sizes have 5 decimals
    let [left, cost] = [size, 0n];
    for (const l of asks) {
      const q = left < l.sz ? left : l.sz;
      cost += q * l.px;
      left -= q;
      if (left === 0n) break;
    }
    const expected = Number(cost) / Number(size) / 1e8;
    send(action(1, ['uint32', 'bool', 'uint64', 'uint64', 'bool', 'uint8', 'uint128'], [0, true, (asks[asks.length - 1].px / 10n ** 8n) * 10n ** 8n, size, false, 3, 0n]));
    const all = (await forkInfo({ type: 'userFills', user: WALLET })).reverse();
    const mine = all.slice(seen);
    seen = all.length;
    const filled = mine.reduce((t, x) => t + Number(x.sz), 0);
    const avg = mine.reduce((t, x) => t + Number(x.px) * Number(x.sz), 0) / filled;
    const fee = mine.reduce((t, x) => t + Number(x.fee), 0);
    const feeExpected = avg * filled * TAKER_FEE;
    fills.push({ what: `IOC buy ${Number(size) / 1e8} BTC (${want})`, levelsTaken: mine.length, avgPx: round(avg, 6), bookAvgPx: round(expected, 6), fee: round(fee, 6), feeAtTakerRate: round(feeExpected, 6), equal: Math.abs(filled - Number(size) / 1e8) < 1e-9 && bps(avg, expected) < 0.001 && Math.abs(fee - feeExpected) < 1e-6 * (mine.length + 1), diffBps: round(Math.max(bps(avg, expected), bps(fee, feeExpected))) });
  }
  result.groups.push(group('Fills against the order book', 'average fill price equals the price of walking the l2Book levels read just before the order, and the fee is 0.045% of the notional, cut to a millionth of a dollar on each level taken', fills));

  // 6. Funding: one hour on the fork's clock against size times oracle price times the rate.
  const stateBefore = chainsim('call', FORK, 'hypercore_state', WALLET);
  const position = stateBefore.clearinghouseState.assetPositions.find((p) => p.position.coin === 'BTC').position;
  const ctx = (await forkInfo({ type: 'metaAndAssetCtxs' }))[1][0];
  chainsim('time', FORK, '--advance', '3600');
  const stateAfter = chainsim('call', FORK, 'hypercore_state', WALLET);
  const paid = Number(stateAfter.clearinghouseState.assetPositions.find((p) => p.position.coin === 'BTC').position.cumFunding.sinceOpen) - Number(position.cumFunding.sinceOpen);
  const expected = Number(position.szi) * Number(ctx.oraclePx) * Number(ctx.funding);
  result.groups.push(group('Funding', 'one hour of funding equals size times oracle price times the hourly rate', [{ what: `BTC long ${position.szi} at rate ${ctx.funding}`, paid: round(paid, 6), rateTimesNotional: round(expected, 6), equal: Math.abs(paid - expected) < 2e-6, diffBps: round(bps(paid, expected)) }]));

  writeFileSync(new URL('fidelity.json', OUT), `${JSON.stringify(result, null, 1)}\n`);
  render();
  const unequal = result.groups.filter((g) => g.equal !== g.total).map((g) => `${g.name}: ${g.equal} of ${g.total}`);
  console.log(JSON.stringify({ groups: result.groups.length, checks: result.groups.reduce((t, g) => t + g.total, 0), notAllEqual: unequal }));
}

function render() {
  const r = JSON.parse(readFileSync(new URL('fidelity.json', OUT), 'utf8'));
  const lines = [
    '# Fork against mainnet',
    '',
    `The fidelity check reads a chainsim fork of HyperEVM mainnet and mainnet itself side by side. This run: ${r.date.slice(0, 10)}, ${r.chainsim}${r.commit ? ` built from commit ${r.commit}` : ''}${r.proxy ? `, proxy ${r.proxy}` : ''}, fork block ${r.forkBlock}, mainnet through the ${r.upstream}.`,
    '',
    'Reproduce it from a clone of the repository, with Docker as the only requirement:',
    '',
    '```sh',
    'examples/hyperliquid/scripts/docker.sh fidelity/run.sh',
    '```',
    '',
    'Every number here comes from [fidelity.json](fidelity.json), which the run writes with one entry per compared value.',
    '',
    '| Group | Equal | Largest difference | Rule |',
    '| --- | --- | --- | --- |',
    ...r.groups.map((g) => `| ${g.name} | ${g.equal} of ${g.total} | ${g.largestDiffBps === null ? 'n/a' : `${g.largestDiffBps} bp`} | ${g.rule} |`),
    '',
    `\`up\` took ${r.groups.find((g) => g.secondsOfUp !== undefined).secondsOfUp} s between the two info API price reads. The second account was mirrored ${r.secondMirrorSecondsAfterUp} s after \`up\`.`,
    '',
  ];
  writeFileSync(new URL('README.md', OUT), lines.join('\n'));
}

if (process.argv[2] === 'render') render();
else await run();
