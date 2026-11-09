// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

interface ICoreWriter {
    function sendRawAction(bytes calldata data) external;
}

/// Reads HyperCore through the HyperEVM read precompiles and writes to it through CoreWriter.
library HyperCore {
    address internal constant POSITION = 0x0000000000000000000000000000000000000800;
    address internal constant MARK_PX = 0x0000000000000000000000000000000000000806;
    address internal constant ORACLE_PX = 0x0000000000000000000000000000000000000807;
    address internal constant CORE_WRITER = 0x3333333333333333333333333333333333333333;

    uint8 internal constant TIF_GTC = 2;
    uint8 internal constant TIF_IOC = 3;

    struct Position {
        int64 szi;
        uint64 entryNtl;
        int64 isolatedRawUsd;
        uint32 leverage;
        bool isIsolated;
    }

    function position(address user, uint16 perp) internal view returns (Position memory) {
        (bool ok, bytes memory out) = POSITION.staticcall(abi.encode(user, perp));
        require(ok, "HyperCore: position read failed");
        return abi.decode(out, (Position));
    }

    function markPx(uint32 perp) internal view returns (uint64) {
        (bool ok, bytes memory out) = MARK_PX.staticcall(abi.encode(perp));
        require(ok, "HyperCore: markPx read failed");
        return abi.decode(out, (uint64));
    }

    function oraclePx(uint32 perp) internal view returns (uint64) {
        (bool ok, bytes memory out) = ORACLE_PX.staticcall(abi.encode(perp));
        require(ok, "HyperCore: oraclePx read failed");
        return abi.decode(out, (uint64));
    }

    /// Action 1. limitPx and sz are in 1e8 units.
    function limitOrderAction(uint32 asset, bool isBuy, uint64 limitPx, uint64 sz, bool reduceOnly, uint8 tif, uint128 cloid)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(uint8(1), uint24(1), abi.encode(asset, isBuy, limitPx, sz, reduceOnly, tif, cloid));
    }

    /// Action 7. ntl is in 1e6 USD units.
    function usdClassTransferAction(uint64 ntl, bool toPerp) internal pure returns (bytes memory) {
        return abi.encodePacked(uint8(1), uint24(7), abi.encode(ntl, toPerp));
    }

    function send(bytes memory action) internal {
        ICoreWriter(CORE_WRITER).sendRawAction(action);
    }
}
