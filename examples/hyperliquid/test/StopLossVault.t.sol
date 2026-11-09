// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {HyperCore} from "../src/HyperCore.sol";
import {StopLossVault} from "../src/StopLossVault.sol";

/// Unit tests on a chainsim fork of HyperEVM mainnet: forge test --fork-url $CHAINSIM_RPC.
/// The read precompiles answer from the fork's HyperCore, seeded from mainnet at `chainsim up`.
/// scripts/test.sh gives VAULT a 0.01 BTC long with `chainsim set hypercore position` first.
contract StopLossVaultTest is Test {
    event RawAction(address indexed user, bytes data);

    address constant VAULT = address(0x5A1E);
    uint32 constant BTC = 0;
    uint8 constant BTC_SZ_DECIMALS = 5;

    StopLossVault vault;

    function setUp() public {
        deployCodeTo("StopLossVault.sol", abi.encode(BTC, BTC_SZ_DECIMALS, uint64(1)), VAULT);
        vault = StopLossVault(VAULT);
    }

    function test_ReadsMainnetPricesThroughThePrecompiles() public view {
        uint64 mark = HyperCore.markPx(BTC);
        uint64 oracle = HyperCore.oraclePx(BTC);
        assertGt(mark, 0);
        // BTC mark and oracle stay within 1% of each other on mainnet.
        assertApproxEqRel(mark, oracle, 0.01e18);
    }

    function test_OpenLongSendsAnIocBuyThroughCoreWriter() public {
        uint64 limitPx = 80_000e8;
        uint64 sz = 0.001e8;
        vm.expectEmit(true, false, false, true, HyperCore.CORE_WRITER);
        emit RawAction(VAULT, HyperCore.limitOrderAction(BTC, true, limitPx, sz, false, HyperCore.TIF_IOC, 0));
        vault.openLong(limitPx, sz);
    }

    function test_OnlyTheOwnerOpens() public {
        vm.prank(address(0xBEEF));
        vm.expectRevert("not owner");
        vault.openLong(80_000e8, 0.001e8);
    }

    function test_HoldsWhileTheMarkIsAboveTheStop() public {
        assertGt(vault.position().szi, 0, "no seeded position: run scripts/test.sh");
        vault.setStop(HyperCore.markPx(BTC) / 2);
        assertFalse(vault.checkAndClose());
    }

    function test_ClosesTheWholeLongAtTheStop() public {
        int64 szi = vault.position().szi;
        assertGt(szi, 0, "no seeded position: run scripts/test.sh");
        uint64 mark = HyperCore.markPx(BTC);
        vault.setStop(mark);
        uint64 limitPx = (mark * 95 / 100 / 10) * 1e8;
        uint64 sz = uint64(szi) * 1e3;
        vm.expectEmit(true, false, false, true, HyperCore.CORE_WRITER);
        emit RawAction(VAULT, HyperCore.limitOrderAction(BTC, false, limitPx, sz, true, HyperCore.TIF_IOC, 0));
        assertTrue(vault.checkAndClose());
    }
}
