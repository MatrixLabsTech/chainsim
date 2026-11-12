// Reads from and submits to a chainsim Aptos fork with the Aptos TypeScript SDK, unchanged:
// the fork's REST URL goes in as the fullnode.
//
//   FULLNODE=<rpcUrl>/v1 SENDER_KEY=<test key> node sdk.mjs address    prints the sender's address
//   FULLNODE=<rpcUrl>/v1 SENDER_KEY=<test key> node sdk.mjs            reads, then sends 0.1 APT
//
// READ_ACCOUNT names the mainnet account to read (default 0x1). Without SENDER_KEY only the
// reads run. The sender needs APT on the fork first: chainsim fund <name> <address> --native 5.
import { Account, Aptos, AptosConfig, Ed25519PrivateKey, Network, generateUserTransactionHash } from "@aptos-labs/ts-sdk";

const sender = process.env.SENDER_KEY && Account.fromPrivateKey({ privateKey: new Ed25519PrivateKey(process.env.SENDER_KEY) });
if (process.argv[2] === "address") {
  console.log(sender.accountAddress.toString());
  process.exit(0);
}
const aptos = new Aptos(new AptosConfig({ network: Network.CUSTOM, fullnode: process.env.FULLNODE }));

const timed = async (f) => {
  const t = performance.now();
  const value = await f();
  return [value, Math.round(performance.now() - t)];
};
const out = { reads: {}, ms: {} };
const who = process.env.READ_ACCOUNT ?? "0x1";

const ledger = await aptos.getLedgerInfo();
out.reads.chainId = ledger.chain_id;
[out.reads.aptBalance, out.ms.balance] = await timed(() => aptos.getAccountAPTAmount({ accountAddress: who }));
[out.reads.resource, out.ms.resource] = await timed(() => aptos.getAccountResource({ accountAddress: who, resourceType: "0x1::account::Account" }));
[out.reads.view, out.ms.view] = await timed(() => aptos.view({ payload: { function: "0x1::coin::supply", typeArguments: ["0x1::aptos_coin::AptosCoin"] } }));

if (sender) {
  const receiver = "0x000000000000000000000000000000000000000000000000000000000000c5e2";
  const amount = 10_000_000;
  const before = await aptos.getAccountAPTAmount({ accountAddress: receiver });
  const transaction = await aptos.transaction.build.simple({
    sender: sender.accountAddress,
    data: { function: "0x1::aptos_account::transfer", functionArguments: [receiver, amount] },
  });
  const [simulated] = await aptos.transaction.simulate.simple({ signerPublicKey: sender.publicKey, transaction });
  out.simulate = { success: simulated.success, gasUsed: simulated.gas_used };
  const [pending, submitMs] = await timed(() => aptos.signAndSubmitTransaction({ signer: sender, transaction }));
  const [done, waitMs] = await timed(() => aptos.waitForTransaction({ transactionHash: pending.hash }));
  const after = await aptos.getAccountAPTAmount({ accountAddress: receiver });
  out.transfer = {
    hash: pending.hash,
    // The hash the SDK computes for the signed transaction is the hash the fork returned.
    hashMatchesSdk: generateUserTransactionHash({ transaction, senderAuthenticator: aptos.transaction.sign({ signer: sender, transaction }) }) === pending.hash,
    success: done.success,
    vmStatus: done.vm_status,
    gasUsed: done.gas_used,
    events: done.events.map((e) => e.type),
    receiverBefore: before,
    receiverAfter: after,
    moved: after - before,
  };
  out.ms.submit = submitMs;
  out.ms.wait = waitMs;
}
console.log(JSON.stringify(out, null, 2));
