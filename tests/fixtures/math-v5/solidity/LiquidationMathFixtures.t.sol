// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.29;

import { Vm } from "lib/forge-std/src/Vm.sol";
import { CreditPoolTestSuite } from "test/CreditPool/0_base.t.sol";

/// Expected outputs are observed public execution, not a copy of the cap formula.
contract LiquidationMathFixtures is CreditPoolTestSuite {
    address internal liquidator = makeAddr("Math Differential Liquidator");
    struct CaseInput {
        string name;
        uint256 numerator;
        uint256 denominator;
        uint128 requested;
        uint128 minimum;
        uint128 collateral;
    }
    struct CaseResult {
        uint256 discount;
        uint256 maximum;
        bool ok;
        bytes revertData;
        uint256 paid;
        uint256 seized;
        uint256 badDebt;
    }

    function reconfigure(InitializationParams memory params) internal pure override {
        params.minDebtRemainingAfterLiquidation = 1;
    }

    function beforeEach() public override {
        _deposit(provider_A, 100e18);
        vm.prank(provider_A); pool.placeProvideOrder(provider_A, markets[0], 0, 100e18);
        _deposit_collateral(borrower_A, markets[0], 250e18);
        vm.prank(borrower_A); pool.borrow(borrower_A, markets[0], 100e18, 100e18);
        _mint(liquidator, 100e18);
    }

    function test_generate_actual_execution_goldens() public {
        string memory json = "[";
        json = string.concat(json, _case("full-capped-88", 44, 100, 100e18, 1, 250e18));
        json = string.concat(json, ",", _case("below-debt-capped-72", 36, 100, type(uint128).max, 1, 250e18));
        json = string.concat(json, ",", _case("partial-44", 44, 100, 44e18, 1, 250e18));
        json = string.concat(json, ",", _case("recovery-equality-not-drain", 6, 250e18, 4, 1, 250e18));
        json = string.concat(json, ",", _case("recovery-plus-one-drain", 6, 250e18, 5, 1, 250e18));
        json = string.concat(json, ",", _case("fractional-partial-not-capped", 69, 2500e18, 5, 1, 250e18));
        json = string.concat(json, ",", _case("double-ceil-drain-small-q", 100e18, 1, 1, 1, 1));
        json = string.concat(json, ",", _case("zero-recovery", 1, 250e18, 1, 1, 250e18));
        json = string.concat(json, ",", _case("dust-equality-allowed", 50, 100, 80e18, 20e18, 250e18));
        json = string.concat(json, ",", _case("dust-minus-one-rejected", 50, 100, 80e18 + 1, 20e18, 250e18));
        json = string.concat(json, ",", _case("dust-drain-exception", 44, 100, 100e18, 20e18, 250e18));
        json = string.concat(json, ",", _case("zero-request", 44, 100, 0, 1, 250e18));
        json = string.concat(json, ",", _case("above-debt-request", 44, 100, 100e18 + 1, 1, 250e18));
        for (uint256 i; i < 128; ++i) {
            uint256 seed = uint256(keccak256(abi.encode("barterlend-liquidation-v5", i)));
            uint256 denominator = 100 + seed % 1e24;
            uint256 numerator = 1 + (seed >> 32) % (denominator * 79 / 100);
            uint128 requested = uint128(1 + (seed >> 64) % 100e18);
            uint128 minimum = i % 4 == 0 ? 0 : i % 4 == 1 ? 1 : i % 4 == 2 ? 10e18 : 20e18;
            json = string.concat(json, ",", _case(string.concat("seed-", vm.toString(i)), numerator, denominator, requested, minimum, 250e18));
        }
        json = string.concat(json, "]");
        vm.writeFile("./generated/actual-liquidations.json", json);
    }

    function test_fuzz_real_liquidation_limits(uint80 numeratorSeed, uint80 denominatorSeed, uint128 requestSeed, uint64 minimumSeed) public {
        uint256 denominator = bound(denominatorSeed, 100, 1e24);
        uint256 numerator = bound(numeratorSeed, 1, denominator * 79 / 100);
        uint128 requested = uint128(bound(requestSeed, 1, 100e18));
        uint128 minimum = uint128(bound(minimumSeed, 0, 20e18));
        _case("fuzz", numerator, denominator, requested, minimum, 250e18);
    }

    function _case(string memory name, uint256 numerator, uint256 denominator, uint128 requested, uint128 minimum, uint128 positionCollateral)
        internal returns (string memory json)
    {
        return _runCase(CaseInput(name, numerator, denominator, requested, minimum, positionCollateral));
    }

    function _runCase(CaseInput memory input) private returns (string memory json) {
        uint256 snapshot = vm.snapshotState();
        if (input.collateral != 250e18) {
            priceOracle.setPriceFraction(400e18, 1);
            vm.prank(borrower_A); pool.withdrawCollateral(borrower_A, markets[0], borrower_A, 250e18 - input.collateral);
        }
        pool.setMinDebtRemainingAfterLiquidation(input.minimum);
        priceOracle.setPriceFraction(input.numerator, input.denominator);
        assertTrue(pool.isLiquidatable(markets[0], borrower_A));
        uint256 health = pool.getHealthFactor(markets[0], borrower_A);
        CaseResult memory result;
        result.discount = pool.getLiquidationDiscountMin(markets[0]) + UNIT - (health < UNIT ? health : UNIT);
        result.maximum = pool.getLiquidationDiscountMax(markets[0]);
        if (result.discount > result.maximum) result.discount = result.maximum;
        uint256 assetBefore = asset.balanceOf(liquidator);
        uint256 collateralBefore = collateral.balanceOf(liquidator);
        vm.recordLogs();
        vm.prank(liquidator); (result.ok, result.revertData) = address(pool).call(abi.encodeWithSelector(
            pool.liquidate.selector, markets[0], borrower_A, input.requested, result.discount, block.timestamp, bytes("")
        ));
        Vm.Log[] memory logs = vm.getRecordedLogs();
        result.paid = assetBefore - asset.balanceOf(liquidator);
        result.seized = collateral.balanceOf(liquidator) - collateralBefore;
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == address(pool) && logs[i].topics[0] == BadDebtIncurred.selector) {
                result.badDebt += abi.decode(logs[i].data, (uint128));
            }
        }
        if (result.ok) {
            uint256 resolved = input.requested == type(uint128).max ? 100e18 : input.requested;
            assertGt(result.paid, 0);
            assertLe(result.paid, resolved);
            assertGt(result.seized, 0);
            assertLe(result.seized, input.collateral);
            assertEq(result.paid + pool.debtOf(markets[0], borrower_A) + result.badDebt, 100e18);
            assertEq(result.seized + pool.collateralOf(markets[0], borrower_A), input.collateral);
        } else {
            assertEq(result.paid, 0);
            assertEq(result.seized, 0);
            assertEq(pool.debtOf(markets[0], borrower_A), 100e18);
            assertEq(pool.collateralOf(markets[0], borrower_A), input.collateral);
            assertGt(result.revertData.length, 0);
        }
        json = string.concat('{"name":"', input.name, '","debt":"100000000000000000000","collateral":"', vm.toString(input.collateral),
            '","requested":"', vm.toString(input.requested), '","minimum_remaining":"', vm.toString(input.minimum));
        json = string.concat(json, '","numerator":"', vm.toString(input.numerator), '","denominator":"', vm.toString(input.denominator),
            '","discount":"', vm.toString(result.discount), '","maximum_discount":"', vm.toString(result.maximum), '","ok":', result.ok ? "true" : "false");
        json = string.concat(json, ',"actual_repay":"', vm.toString(result.paid), '","seized":"', vm.toString(result.seized),
            '","remaining_debt":"', vm.toString(pool.debtOf(markets[0], borrower_A)),
            '","remaining_collateral":"', vm.toString(pool.collateralOf(markets[0], borrower_A)),
            '","bad_debt":"', vm.toString(result.badDebt), '","revert_data":"', vm.toString(result.revertData), '"}');
        assertTrue(vm.revertToState(snapshot));
    }
}
