// End-to-end test of StopLossVault on a chainsim fork of HyperEVM mainnet, with viem.
// Real transactions go through the fork's RPC proxy, which applies every CoreWriter action to the
// fork's HyperCore after each block. Orders fill against the mainnet order book taken at `up`.
//   CHAINSIM_RPC   the fork's rpcUrl (default http://hyperevm-mainnet:8545)
//   CHAINSIM_FORK  the fork's name (default hyperevm-mainnet)
//   E2E_KEY        a test key with no mainnet history (chainsim fund refuses used addresses)
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createPublicClient, createWalletClient, defineChain, http, decodeAbiParameters, encodeAbiParameters } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';

const RPC = process.env.CHAINSIM_RPC ?? 'http://hyperevm-mainnet:8545';
const FORK = process.env.CHAINSIM_FORK ?? 'hyperevm-mainnet';
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
const markPx = async () => decodeAbiParameters([{ type: 'uint64' }], (await pub.call({ to: '0x0000000000000000000000000000000000000806', data: encodeAbiParameters([{ type: 'uint32' }], [0]) })).data)[0];
const szi = async (vault) => (await pub.readContract({ address: vault, abi, functionName: 'position' })).szi;
async function until(f, ms = 5000) { const end = Date.now() + ms; let v; while (!(v = await f()) && Date.now() < end) await new Promise((r) => setTimeout(r, 50)); return v; }

// 1. A funded deployer and a fresh vault for BTC (perp 0, szDecimals 5).
chainsim('fund', FORK, account.address, '--native', '10');
const deploy = await pub.waitForTransactionReceipt({ hash: await wallet.deployContract({ abi, bytecode: bytecode.object, args: [0, 5, 0n] }) });
const vault = deploy.contractAddress;
check(deploy.status === 'success', `deployed StopLossVault at ${vault}`);

// 2. The vault holds 1,000 USDC on Core spot and moves 500 to perp through CoreWriter.
chainsim('set', FORK, 'hypercore', 'spot-balance', vault, '0', '100000000000');
await send(vault, 'fundPerp', [500_000_000n]);
const ch = await until(async () => { const s = await info({ type: 'clearinghouseState', user: vault }); return Number(s.withdrawable) > 0 && s; });
check(ch && Number(ch.withdrawable) === 500, `perp balance 500 USD (/info clearinghouseState withdrawable ${ch?.withdrawable})`);

// 3. Open a 0.01 BTC long with an IOC 1% over the best ask: it fills against the mainnet book.
const book = await info({ type: 'l2Book', coin: 'BTC' });
const ask = Number(book.levels[1][0].px);
await send(vault, 'openLong', [BigInt(Math.round(ask * 1.01)) * 10n ** 8n, 1_000_000n]);
const opened = await until(async () => (await szi(vault)) === 1000n);
check(opened, `position 0.01 BTC read back through the 0x800 precompile (best ask ${ask})`);
const fills = await info({ type: 'userFills', user: vault });
check(fills.length === 1 && fills[0].dir === 'Open Long', `/info userFills: ${fills[0]?.dir} ${fills[0]?.sz} at ${fills[0]?.px}`);

// 4. Stop 2% under the mark. The keeper call does nothing while the mark is above it.
const mark = await markPx();
await send(vault, 'setStop', [(mark * 98n) / 100n]);
const { result: holds } = await pub.simulateContract({ address: vault, abi, functionName: 'checkAndClose', account });
check(holds === false, `keeper holds at mark ${mark}, stop ${(mark * 98n) / 100n}`);

// 5. Move BTC 3% down on the fork. The keeper call closes the long with a reduce-only IOC.
chainsim('set', FORK, 'hypercore', 'price', '0', String((mark * 97n) / 100n));
const rc = await send(vault, 'checkAndClose');
check(rc.logs.length > 0, `keeper closed at the stop (tx ${rc.transactionHash.slice(0, 10)})`);
const closed = await until(async () => (await szi(vault)) === 0n);
check(closed, 'position reads 0 through the 0x800 precompile');
const after = await info({ type: 'userFills', user: vault });
const closes = after.filter((f) => f.dir === 'Close Long');
const closedSz = closes.reduce((t, f) => t + Number(f.sz), 0);
check(Math.abs(closedSz - 0.01) < 1e-9, `/info userFills: Close Long ${closedSz.toFixed(2)} BTC in ${closes.length} fill${closes.length === 1 ? '' : 's'}, first at ${closes[0]?.px}`);
const actions = chainsim('call', FORK, 'corewriter_actions').filter((a) => a.user.toLowerCase() === vault.toLowerCase());
check(actions.length === 3, `corewriter_actions: ${actions.map((a) => a.result.split(',')[0]).join(', ')}`);

console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
