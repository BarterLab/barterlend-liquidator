// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

// Separate build entry point; upstream sources and fixtures remain unchanged.
import { CreditPool } from "src/CreditPool.sol";
import { CreditPoolFacet1 } from "src/facets/CreditPoolFacet1.sol";
import { CreditPoolFacet2 } from "src/facets/CreditPoolFacet2.sol";
import { CreditPoolFacet3 } from "src/facets/CreditPoolFacet3.sol";
import { CreditPoolFacet4 } from "src/facets/CreditPoolFacet4.sol";
import { AaveV3Adapter } from "src/adapters/AaveV3Adapter.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";
import { TestnetToken, TestnetWrappedMON, TestnetOracle, TestnetAaveReceipt, TestnetAavePool, TestnetV3Bootstrap } from "./TestnetDependencies.sol";

import { TestLendingFactory } from "../fixtures/factory/TestLendingFactory.sol";
