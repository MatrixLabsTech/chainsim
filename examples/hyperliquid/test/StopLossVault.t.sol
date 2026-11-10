// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {Test, Vm} from "forge-std/Test.sol";
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

    /// Over mark prices, stops and position sizes: the keeper call sends nothing while the mark is
    /// above the stop or there is no long, and otherwise one reduce-only IOC sell of the whole
    /// position with a limit at or under the stop. Marks run from 100 USD to 1,000,000,000 USD in
    /// the precompile's units, the range in which the vault's uint64 limit price is exact.
    function testFuzz_ClosesTheWholeLongOnlyAtOrUnderTheStop(uint64 mark, uint64 stop, int64 szi) public {
        mark = uint64(bound(mark, 1_000, 1e10));
        stop = uint64(bound(stop, 1_000, 1e10));
        szi = int64(bound(szi, -1e12, 1e12));
        vm.mockCall(HyperCore.MARK_PX, abi.encode(BTC), abi.encode(mark));
        vm.mockCall(HyperCore.POSITION, abi.encode(VAULT, uint16(BTC)), abi.encode(HyperCore.Position(szi, 0, 0, 20, false)));
        vault.setStop(stop);

        vm.recordLogs();
        bool closed = vault.checkAndClose();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        if (szi <= 0 || mark > stop) {
            assertFalse(closed);
            assertEq(logs.length, 0, "an action was sent above the stop");
            return;
        }
        assertTrue(closed);
        assertEq(logs.length, 2);
        assertEq(logs[0].emitter, HyperCore.CORE_WRITER);
        bytes memory action = abi.decode(logs[0].data, (bytes));
        assertEq(bytes4(action), bytes4(0x01000001), "not a limit order action");
        bytes memory fields = new bytes(action.length - 4);
        for (uint256 i = 0; i < fields.length; i++) fields[i] = action[i + 4];
        (uint32 asset, bool isBuy, uint64 limitPx, uint64 sz, bool reduceOnly, uint8 tif,) =
            abi.decode(fields, (uint32, bool, uint64, uint64, bool, uint8, uint128));
        assertEq(asset, BTC);
        assertFalse(isBuy);
        assertTrue(reduceOnly);
        assertEq(tif, HyperCore.TIF_IOC);
        assertEq(sz, uint64(szi) * 1e3, "not the whole position");
        assertGt(limitPx, 0);
        // The precompile's BTC price has one decimal and the order's has eight.
        assertLe(limitPx, uint256(stop) * 1e7, "limit above the stop");
        assertLe(limitPx, uint256(mark) * 1e7);
    }
}
