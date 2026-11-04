// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

interface IKyberRouter {
    struct Description {
        address srcToken; address dstToken; address[] srcReceivers; uint256[] srcAmounts;
        address[] feeReceivers; uint256[] feeAmounts; address dstReceiver;
        uint256 amount; uint256 minReturnAmount; uint256 flags; bytes permit;
    }
    struct Execution { address callTarget; address approveTarget; bytes targetData; Description desc; bytes clientData; }
    function swap(Execution calldata execution) external payable returns (uint256 returnAmount, uint256 gasUsed);
}

/// @dev Versioned ERC20-only MetaAggregationRouterV2 calldata. No delegatecall,
/// permit, native value, partial fill or unreviewed selector can enter the executor.
library KyberRoute {
    bytes4 internal constant MAGIC = 0x4b594252;
    error InvalidKyberRoute();

    function decode(bytes memory path, address input, address output, address recipient,
        uint256 expectedAmount, uint256 minimum) internal pure returns (address router, bytes memory callData, uint256 amount)
    {
        if (path.length < 128 || path.length > 65_536) revert InvalidKyberRoute();
        bytes4 magic;
        (magic, router, callData) = abi.decode(path, (bytes4, address, bytes));
        if (magic != MAGIC || router == address(0) || callData.length < 4
            || keccak256(path) != keccak256(abi.encode(magic, router, callData))) revert InvalidKyberRoute();
        bytes4 selector;
        assembly { selector := mload(add(callData, 32)) }
        if (selector != IKyberRouter.swap.selector) revert InvalidKyberRoute();
        bytes memory args = new bytes(callData.length - 4);
        assembly ("memory-safe") { mcopy(add(args, 32), add(callData, 36), mload(args)) }
        IKyberRouter.Execution memory execution = abi.decode(args, (IKyberRouter.Execution));
        if (keccak256(callData) != keccak256(abi.encodeWithSelector(selector, execution))) revert InvalidKyberRoute();
        IKyberRouter.Description memory d = execution.desc;
        amount = d.amount;
        if (d.srcToken != input || d.dstToken != output || input == output
            || (d.dstReceiver != recipient && d.dstReceiver != address(0))
            || amount == 0 || (expectedAmount != 0 && amount != expectedAmount)
            || d.minReturnAmount == 0 || d.minReturnAmount < minimum || d.permit.length != 0
            || d.flags & 0x23 != 0 || d.feeReceivers.length != 0 || d.feeAmounts.length != 0
            || execution.callTarget == address(0) || execution.callTarget == recipient
            || execution.approveTarget == recipient || execution.targetData.length == 0
            || d.srcReceivers.length != d.srcAmounts.length || d.srcReceivers.length > 64) revert InvalidKyberRoute();
        uint256 distributed;
        for (uint256 i; i < d.srcAmounts.length; ++i) {
            if (d.srcReceivers[i] == address(0) || d.srcReceivers[i] == recipient) revert InvalidKyberRoute();
            distributed += d.srcAmounts[i];
        }
        if (distributed != amount) revert InvalidKyberRoute();
    }
}
