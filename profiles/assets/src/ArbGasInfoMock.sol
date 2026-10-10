// SPDX-License-Identifier: MIT OR Apache-2.0
//
// ArbGasInfo mock for the robinhood-mainnet profile. chainsim installs its
// runtime code at 0x6C with anvil_setCode at up, because stock anvil reverts
// there (design section 3). Every function returns a constant measured on
// Robinhood Chain mainnet (chain 4663) at block 85104168 on 2026-10-10 with
// `cast call`. Functions not listed here revert.
//
// Build, from the repo root, in the pinned foundry image (the lock's foundry
// digest), and write the output, without a newline, to
// profiles/assets/arbgasinfo-mock.hex:
//   docker run --rm -v "$PWD/profiles/assets/src":/in:ro --entrypoint sh \
//     ghcr.io/foundry-rs/foundry:v1.8.5 -c 'cd /tmp && mkdir -p b/src && cd b && \
//     cp /in/ArbGasInfoMock.sol src/ && forge inspect src/ArbGasInfoMock.sol:ArbGasInfoMock \
//     deployedBytecode --use 0.8.30 --optimize' 
pragma solidity 0.8.30;

contract ArbGasInfoMock {
    function getPricesInWei() external pure returns (uint256, uint256, uint256, uint256, uint256, uint256) {
        return (571365760, 4081184, 416200000000, 20000000, 810000, 20810000);
    }

    function getPricesInWeiWithAggregator(address) external pure returns (uint256, uint256, uint256, uint256, uint256, uint256) {
        return (571365760, 4081184, 416200000000, 20000000, 810000, 20810000);
    }

    function getPricesInArbGas() external pure returns (uint256, uint256, uint256) {
        return (27, 0, 20000);
    }

    function getPricesInArbGasWithAggregator(address) external pure returns (uint256, uint256, uint256) {
        return (27, 0, 20000);
    }

    function getGasAccountingParams() external pure returns (uint256, uint256, uint256) {
        return (7000000, 32000000, 32000000);
    }

    function getMinimumGasPrice() external pure returns (uint256) {
        return 20000000;
    }

    function getL1BaseFeeEstimate() external pure returns (uint256) {
        return 255074;
    }

    function getL1GasPriceEstimate() external pure returns (uint256) {
        return 255074;
    }

    function getL1BaseFeeEstimateInertia() external pure returns (uint64) {
        return 10;
    }

    function getL1RewardRate() external pure returns (uint64) {
        return 10;
    }

    function getL1RewardRecipient() external pure returns (address) {
        return 0x8F516B9921021522D74d08B94B9f1c22B4Af82ea;
    }

    function getCurrentTxL1GasFees() external pure returns (uint256) {
        return 585162000;
    }

    function getPricingInertia() external pure returns (uint64) {
        return 102;
    }

    function getGasBacklog() external pure returns (uint64) {
        return 0;
    }

    function getGasBacklogTolerance() external pure returns (uint64) {
        return 10;
    }

    function getL1PricingSurplus() external pure returns (int256) {
        return 328202761204872;
    }

    function getAmortizedCostCapBips() external pure returns (uint64) {
        return 0;
    }
}
