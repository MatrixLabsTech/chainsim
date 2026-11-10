// A real past day against StopLossVault on a chainsim fork of HyperEVM mainnet, with viem.
// The fork replays BTC's hourly mainnet candles of one day. The vault opens a long at the day's
// first prices with a stop 3% under its entry, and a keeper calls checkAndClose after every two
// candles, as a bot would. The order book follows each candle, so the close fills at that hour's
// level.
//   CHAINSIM_RPC   the fork's rpcUrl (default http://hyperevm-mainnet:8545)
//   CHAINSIM_FORK  the fork's name (default hyperevm-mainnet)
//   E2E_KEY        a test key with no mainnet history
//   REPLAY_DAY     the UTC day to replay (default 2026-11-05, when BTC fell 6% within the day)
//   E2E_RESULTS    a JSON file to add this run's numbers to
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { createPublicClient, createWalletClient, defineChain, http } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';

const RPC = process.env.CHAINSIM_RPC ?? 'http://hyperevm-mainnet:8545';
const FORK = process.env.CHAINSIM_FORK ?? 'hyperevm-mainnet';
const DAY = process.env.REPLAY_DAY ?? '2026-11-05';
const account = privateKeyToAccount(process.env.E2E_KEY);
const chain = defineChain({ id: 999, name: 'HyperEVM fork', nativeCurrency: { name: 'HYPE', symbol: 'HYPE', decimals: 18 }, rpcUrls: { default: { http: [RPC] } } });
const pub = createPublicClient({ chain, transport: http(RPC) });
const wallet = createWalletClient({ chain, account, transport: http(RPC) });
const { abi, bytecode } = JSON.parse(readFileSync(new URL('../out/StopLossVault.sol/StopLossVault.json', import.meta.url)));

let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}`); if (!ok) failed++; };
const chainsim = (...args) => {
  const out = JSON.parse(execFileSync('chainsim', args, { encoding: 'utf8' }));
  if (!out.ok) throw new Error(`chainsim ${args.join(' ')}: ${out.error.code}`);
  return out.data;
};
const info = async (body) => (await fetch(`${RPC}/info`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })).json();
const send = async (address, functionName, args = []) => pub.waitForTransactionReceipt({ hash: await wallet.writeContract({ address, abi, functionName, args }) });
const szi = async (vault) => (await pub.readContract({ address: vault, abi, functionName: 'position' })).szi;
async function until(f, ms = 5000) { const end = Date.now() + ms; let v; while (!(v = await f()) && Date.now() < end) await new Promise((r) => setTimeout(r, 50)); return v; }
const hour = (h) => `${DAY}T${String(h).padStart(2, '0')}:00Z`;
const replay = (from, to) => chainsim('time', FORK, '--replay', 'BTC', '--from', hour(from), '--to', hour(to));

// 1. The day's first two candles put the fork's BTC price and order book at that day's level.
const first = replay(0, 1);
check(first.candles === 2, `replayed ${DAY} 00:00 and 01:00, BTC at ${first.lastClose}`);

// 2. A fresh vault with 500 USD on perp opens a 0.01 BTC long against the moved book.
chainsim('fund', FORK, account.address, '--native', '10');
const deploy = await pub.waitForTransactionReceipt({ hash: await wallet.deployContract({ abi, bytecode: bytecode.object, args: [0, 5, 0n] }) });
const vault = deploy.contractAddress;
chainsim('set', FORK, 'hypercore', 'spot-balance', vault, '0', '100000000000');
await send(vault, 'fundPerp', [500_000_000n]);
await until(async () => Number((await info({ type: 'clearinghouseState', user: vault })).withdrawable) > 0);
const ask = Number((await info({ type: 'l2Book', coin: 'BTC' })).levels[1][0].px);
await send(vault, 'openLong', [BigInt(Math.round(ask * 1.01)) * 10n ** 8n, 1_000_000n]);
check(await until(async () => (await szi(vault)) === 1000n), `opened 0.01 BTC long, best ask ${ask}`);
// The open may take more than one book level: the entry is the average over its fills.
const opens = (await info({ type: 'userFills', user: vault })).filter((f) => f.dir === 'Open Long');
const entry = opens.reduce((t, f) => t + Number(f.px) * Number(f.sz), 0) / opens.reduce((t, f) => t + Number(f.sz), 0);
const stop = BigInt(Math.round(entry * 0.97 * 10));
await send(vault, 'setStop', [stop]);
check(Math.abs(entry - Number(first.lastClose)) / Number(first.lastClose) < 0.005, `entry ${entry} within 0.5% of the 01:00 close, stop ${Number(stop) / 10}`);

// 3. The rest of the day, two candles at a time, with the keeper call after each step.
let closedAt = null;
let low = Number(first.lastClose);
for (let h = 2; h <= 22; h += 2) {
  const step = replay(h, h + 1);
  low = Math.min(low, Number(step.lowClose));
  const { result: wouldClose } = await pub.simulateContract({ address: vault, abi, functionName: 'checkAndClose', account });
  if (closedAt === null && wouldClose) {
    await send(vault, 'checkAndClose');
    await until(async () => (await szi(vault)) === 0n);
    closedAt = { hour: h + 1, mark: Number(step.lastClose) };
  }
}
check(closedAt !== null, closedAt ? `keeper closed after the ${hour(closedAt.hour)} candle, BTC at ${closedAt.mark}` : `the mark never reached the stop, day low ${low}`);
check((await szi(vault)) === 0n, 'position reads 0 through the 0x800 precompile at the end of the day');

// 4. The close filled under the stop, at that hour's book, and the loss is the price difference.
const closes = (await info({ type: 'userFills', user: vault })).filter((f) => f.dir === 'Close Long');
const closePx = closes.reduce((t, f) => t + Number(f.px) * Number(f.sz), 0) / closes.reduce((t, f) => t + Number(f.sz), 0);
const pnl = closes.reduce((t, f) => t + Number(f.closedPnl), 0);
check(closePx < Number(stop) / 10 && closedAt && Math.abs(closePx - closedAt.mark) / closedAt.mark < 0.005, `close filled at ${closePx}, under the stop and within 0.5% of the candle's close`);
check(Math.abs(pnl - (closePx - entry) * 0.01) < 0.01, `closed PnL ${pnl.toFixed(2)} USD on 0.01 BTC, day low ${low}`);

if (process.env.E2E_RESULTS) {
  const all = existsSync(process.env.E2E_RESULTS) ? JSON.parse(readFileSync(process.env.E2E_RESULTS, 'utf8')) : {};
  all.replayDay = { day: DAY, entryPx: entry, stopPx: Number(stop) / 10, closedAfterCandle: closedAt && hour(closedAt.hour), closePx, closedPnlUsd: Number(pnl.toFixed(6)), dayLowClose: low };
  writeFileSync(process.env.E2E_RESULTS, `${JSON.stringify(all, null, 1)}\n`);
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
