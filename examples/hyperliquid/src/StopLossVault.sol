// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.24;

import {HyperCore} from "./HyperCore.sol";

/// A vault that holds one perp long on HyperCore and closes it when the mark price falls to a stop.
/// The owner opens the position. Anyone, such as a keeper bot, may call checkAndClose.
contract StopLossVault {
    address public immutable owner;
    uint32 public immutable perp;
    uint8 public immutable szDecimals;
    uint64 public stopPx; // in the precompile's raw price units, 10^(6 - szDecimals)

    event Opened(uint64 limitPx, uint64 sz);
    event StopTriggered(uint64 markPx, int64 szi, uint64 limitPx);

    constructor(uint32 perp_, uint8 szDecimals_, uint64 stopPx_) {
        owner = msg.sender;
        perp = perp_;
        szDecimals = szDecimals_;
        stopPx = stopPx_;
    }

    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    function setStop(uint64 stopPx_) external onlyOwner {
        stopPx = stopPx_;
    }

    /// Moves USD from the vault's Core spot balance to its perp balance.
    function fundPerp(uint64 usd) external onlyOwner {
        HyperCore.send(HyperCore.usdClassTransferAction(usd, true));
    }

    /// An IOC buy. limitPx and sz are in 1e8 units, as CoreWriter takes them.
    function openLong(uint64 limitPx, uint64 sz) external onlyOwner {
        HyperCore.send(HyperCore.limitOrderAction(perp, true, limitPx, sz, false, HyperCore.TIF_IOC, 0));
        emit Opened(limitPx, sz);
    }

    function position() public view returns (HyperCore.Position memory) {
        return HyperCore.position(address(this), uint16(perp));
    }

    /// Closes the whole long with a reduce-only IOC sell 5% under the mark once the mark is at or
    /// under the stop. Returns whether it sent the close.
    function checkAndClose() external returns (bool) {
        uint64 mark = HyperCore.markPx(perp);
        int64 szi = position().szi;
        if (szi <= 0 || mark > stopPx) return false;
        uint64 unit = uint64(10 ** (6 - szDecimals));
        uint64 limitPx = (mark * 95 / 100 / unit) * 1e8; // whole dollars, 1e8 units
        uint64 sz = uint64(szi) * uint64(10 ** (8 - szDecimals));
        HyperCore.send(HyperCore.limitOrderAction(perp, false, limitPx, sz, true, HyperCore.TIF_IOC, 0));
        emit StopTriggered(mark, szi, limitPx);
        return true;
    }
}
