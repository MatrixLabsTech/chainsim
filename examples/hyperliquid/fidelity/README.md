# Fork against mainnet

The fidelity check reads a chainsim fork of HyperEVM mainnet and mainnet itself side by side. This run: 2026-11-10, chainsim 0.1.12, proxy 0.1.12-b7a748f580a6, fork block 50927397, mainnet through the public RPC.

Reproduce it from a clone of the repository, with Docker as the only requirement:

```sh
examples/hyperliquid/scripts/docker.sh fidelity/run.sh
```

Every number here comes from [fidelity.json](fidelity.json), which the run writes with one entry per compared value.

| Group | Equal | Largest difference | Rule |
| --- | --- | --- | --- |
| Price precompile reads | 8 of 8 | 0 bp | between the info API price read just before up and the one read just after, or within 1 bp of one of them |
| HLP strategy vault: positions mirrored | 1 of 1 | 0 bp | every position the info API lists is on the fork |
| HLP strategy vault: position szi | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| HLP strategy vault: position entryPx | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| HLP strategy vault: position leverage | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| HLP strategy vault: position maxLeverage | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| HLP strategy vault: position cumFunding | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| HLP strategy vault: position positionValue | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| HLP strategy vault: position marginUsed | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| HLP strategy vault: position unrealizedPnl | 177 of 177 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| HLP strategy vault: position entry notional | 177 of 177 | 0 bp | position value less unrealized PnL, within 0.01 USD of a value the info API gave while the mirror ran |
| HLP strategy vault: position liquidationPx | 100 of 100 | 0.0008 bp | within the range the info API gave while the mirror ran, or within 1 bp of it |
| HLP strategy vault: positions with no liquidation price | 77 of 77 | 0 bp | null on the fork where the info API gives null |
| HLP strategy vault: account summary | 6 of 6 | 0 bp | within the range the info API gave while the mirror ran, or within 1 bp of it |
| Account on standard margin: positions mirrored | 1 of 1 | 0 bp | every position the info API lists is on the fork |
| Account on standard margin: position szi | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| Account on standard margin: position entryPx | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| Account on standard margin: position leverage | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| Account on standard margin: position maxLeverage | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| Account on standard margin: position cumFunding | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran |
| Account on standard margin: position positionValue | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| Account on standard margin: position marginUsed | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| Account on standard margin: position unrealizedPnl | 106 of 106 | 0 bp | equal to a value the info API gave for the position while the mirror ran, to six decimals |
| Account on standard margin: position entry notional | 106 of 106 | 0 bp | position value less unrealized PnL, within 0.01 USD of a value the info API gave while the mirror ran |
| Account on standard margin: position liquidationPx | 62 of 62 | 0 bp | within the range the info API gave while the mirror ran, or within 1 bp of it |
| Account on standard margin: positions with no liquidation price | 44 of 44 | 0 bp | null on the fork where the info API gives null |
| Account on standard margin: account summary | 6 of 6 | 0 bp | within the range the info API gave while the mirror ran, or within 1 bp of it |
| Static precompile reads | 7 of 7 | 0 bp | the same eth_call on the fork and on mainnet returns the same bytes |
| HLP strategy vault: position precompile (0x800) | 11 of 11 | 0 bp | size, leverage and margin mode equal on the fork and on mainnet, for the 20 largest positions, less those mainnet traded while the mirror ran or since |
| Account on standard margin: position precompile (0x800) | 20 of 20 | 0 bp | size, leverage and margin mode equal on the fork and on mainnet, for the 20 largest positions, less those mainnet traded while the mirror ran or since |
| Fills against the order book | 3 of 3 | 0.2861 bp | average fill price equals the price of walking the l2Book levels read just before the order, and the fee is 0.045% of the notional, cut to a millionth of a dollar on each level taken |
| Funding | 1 of 1 | 0.0028 bp | one hour of funding equals size times oracle price times the hourly rate |

`up` took 17.7 s between the two info API price reads. The second account was mirrored 125.1 s after `up`.
