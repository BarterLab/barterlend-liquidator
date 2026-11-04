// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { Script } from "forge-std/Script.sol";
import { LiquidationExecutor } from "../contracts/LiquidationExecutor.sol";

/// @notice Deploy an executor with explicit chain and role settings.
contract DeployLiquidationExecutor is Script {
    function run() external returns (LiquidationExecutor deployed) {
        uint256 expectedChain=vm.envUint("EXECUTOR_CHAIN_ID");
        require(expectedChain != 0 && block.chainid == expectedChain,"chain identity mismatch");
        address owner=vm.envAddress("EXECUTOR_OWNER");
        address operator=vm.envAddress("EXECUTOR_OPERATOR");
        address router=vm.envAddress("V3_ROUTER");
        address factory=vm.envAddress("V3_FACTORY");
        address[] memory lendingFactories=vm.envAddress("LENDING_FACTORIES", ",");
        // Select the signer externally through Foundry.
        vm.startBroadcast();
        deployed=new LiquidationExecutor(owner,operator,router,factory,lendingFactories);
        vm.stopBroadcast();
        // Token, fee and intermediate permissions are initially disabled.
    }
}
