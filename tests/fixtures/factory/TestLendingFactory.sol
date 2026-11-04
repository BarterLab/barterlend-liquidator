// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

/// @dev Test-only factory. Production uses BarterLend's reviewed CreditPoolFactory.
contract TestLendingFactory {
    address public immutable owner;
    mapping(address => bool) public isPool;
    event PoolCreated(address indexed pool);
    constructor(address owner_) { owner = owner_; }
    function poolVersion() external pure returns (uint256) { return 5; }
    function previousFactory() external pure returns (address) { return address(0); }

    // Imports an existing legacy seed or an adversarial fixture, never used by the bot.
    function registerPool(address pool) external {
        require(msg.sender == owner && pool.code.length != 0);
        isPool[pool] = true;
    }

    function createPool(bytes memory initCode) external returns (address pool) {
        require(msg.sender == owner);
        assembly ("memory-safe") { pool := create(0, add(initCode, 32), mload(initCode)) }
        require(pool != address(0) && pool.code.length != 0);
        isPool[pool] = true;
        emit PoolCreated(pool);
    }
}
