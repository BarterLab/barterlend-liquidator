// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ExecutorFixture } from "./ExecutorFixture.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";

contract ExecutorUpstreamCompatibility is ExecutorFixture {
    function test_admin_grace_cut_does_not_change_cached_market_execution_boundary() public {
        _openProvided(borrower_A);
        vm.warp(uint256(markets[0])+1 days-1);
        priceOracle.touch();
        vm.prank(fixtureOwner); pool.setLiquidationGracePeriod(0);
        assertEq(pool.getLiquidationGracePeriod(markets[0]),1 days);
        assertFalse(pool.isLiquidatable(markets[0],borrower_A));
        vm.expectRevert(NotLiquidatable.selector);
        _execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        vm.warp(uint256(markets[0])+1 days);
        priceOracle.touch();
        (uint128 repaid,,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,110e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
    }

    function test_fundable_partial_can_worsen_health_without_dust_or_writeoff() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.1e18);
        assertEq(pool.getHealthFactor(markets[0],borrower_A),181818181818181818);
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(_request(address(pool),borrower_A,10e18,_path(3000),1));
        assertEq(repaid,10e18);
        assertEq(seized,125e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),100e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),275e18);
        assertEq(pool.getHealthFactor(markets[0],borrower_A),137500000000000000);
        assertTrue(pool.isLiquidatable(markets[0],borrower_A));
        assertEq(pool.getMarketLoan(markets[0]),110e18);
    }

    function test_drained_collateral_writes_off_subminimum_remainder_after_actual_repayment() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.32e18);
        vm.expectEmit(true,true,true,true,address(pool));
        emit Liquidated(address(executor),borrower_A,markets[0],102.4e18,400e18);
        vm.expectEmit(true,true,false,true,address(pool));
        emit BadDebtIncurred(borrower_A,markets[0],7.6e18);
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,102.4e18);
        assertEq(seized,400e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
        assertEq(pool.collateralOf(markets[0],borrower_A),0);
        assertEq(pool.getMarketBalance(markets[0]),102.4e18);
        assertEq(pool.getMarketLoan(markets[0]),102.4e18);
        assertEq(pool.loanOf(markets[0],provider_A),102.4e18);
    }

    function test_bad_debt_loss_is_shared_by_two_lenders_and_claimable_after_other_debt_repaid() public {
        _openProvided(borrower_A);
        _deposit_collateral(borrower_B,markets[0],400e18);
        vm.prank(borrower_B); pool.placeBorrowOrder(borrower_B,markets[0],1000,100e18);
        _deposit(provider_B,100e18);
        vm.prank(provider_B); pool.provideExactIn(provider_B,markets[0],100e18,0,10);
        assertEq(pool.loanOf(markets[0],provider_A),110e18);
        assertEq(pool.loanOf(markets[0],provider_B),110e18);
        priceOracle.setPrice(0.1e18);
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,32e18);
        assertEq(seized,400e18);
        assertGt(profit,0);
        assertEq(pool.getMarketLoan(markets[0]),142e18);
        assertEq(pool.loanOf(markets[0],provider_A),71e18);
        assertEq(pool.loanOf(markets[0],provider_B),71e18);
        _deposit(borrower_B,110e18);
        vm.prank(borrower_B); pool.repay(borrower_B,borrower_B,markets[0],110e18);
        vm.warp(markets[0]);
        vm.prank(provider_A); pool.claim(provider_A,markets[0],type(uint128).max);
        vm.prank(provider_B); pool.claim(provider_B,markets[0],type(uint128).max);
        assertEq(pool.balanceOf(provider_A),71e18);
        assertEq(pool.balanceOf(provider_B),71e18);
        assertEq(pool.loanOf(markets[0],provider_A),0);
        assertEq(pool.loanOf(markets[0],provider_B),0);
        assertEq(pool.getMarketBalance(markets[0]),0);
    }

    function test_executor_and_lending_accept_current_block_deadline_exactly() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        LiquidationExecutor.Request memory request=_request(address(pool),borrower_A,110e18,_path(3000),1);
        request.deadline=block.timestamp;
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(request);
        assertEq(repaid,110e18);
        assertEq(seized,343.75e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
        assertEq(asset.allowance(address(executor),address(pool)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }
}
