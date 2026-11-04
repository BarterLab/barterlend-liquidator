// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ExecutorFixture, IFixtureQuoter, IFixtureV3Pool } from "./ExecutorFixture.sol";
import { MockCreditPool } from "test/CreditPool/0_base.t.sol";
import { MockPriceOracle } from "test/mocks/MockPriceOracle.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";

contract ForceNative {
    constructor(address recipient) payable { selfdestruct(payable(recipient)); }
}

contract ExecutorRealProtocol is ExecutorFixture {
    struct AtomicSnapshot {
        uint128 debt;
        uint128 pledged;
        uint256 marketBalance;
        uint256 marketLoan;
        uint256[2] allowances;
        uint256[3][10] balances;
        bytes32[7] slot0;
        uint128[7] liquidity;
    }

    function _atomicSnapshot(address borrower) internal view returns(AtomicSnapshot memory state) {
        state.debt=pool.debtOf(markets[0],borrower);
        state.pledged=pool.collateralOf(markets[0],borrower);
        state.marketBalance=pool.getMarketBalance(markets[0]);
        state.marketLoan=pool.getMarketLoan(markets[0]);
        state.allowances=[asset.allowance(address(executor),address(pool)),collateral.allowance(address(executor),router)];
        address[10] memory accounts=[address(pool),address(executor),router,directPool,lowFeePool,firstHop,secondHop,thinDirect,thinFirst,thinSecond];
        for(uint256 i;i<accounts.length;++i) {
            state.balances[i]=[asset.balanceOf(accounts[i]),collateral.balanceOf(accounts[i]),wrapped.balanceOf(accounts[i])];
            if(i>=3) {
                (bool ok,bytes memory encoded)=accounts[i].staticcall(abi.encodeWithSignature("slot0()"));
                require(ok && encoded.length==224,"canonical V3 slot0 unavailable");
                state.slot0[i-3]=keccak256(encoded);
                state.liquidity[i-3]=IFixtureV3Pool(accounts[i]).liquidity();
            }
        }
    }

    function _assertAtomicSnapshot(AtomicSnapshot memory beforeState,address borrower) internal view {
        AtomicSnapshot memory afterState=_atomicSnapshot(borrower);
        assertEq(afterState.debt,beforeState.debt,"borrower debt changed after revert");
        assertEq(afterState.pledged,beforeState.pledged,"borrower collateral changed after revert");
        assertEq(afterState.marketBalance,beforeState.marketBalance,"market balance changed after revert");
        assertEq(afterState.marketLoan,beforeState.marketLoan,"market loan assets changed after revert");
        for(uint256 i;i<beforeState.allowances.length;++i) assertEq(afterState.allowances[i],beforeState.allowances[i],"allowance changed after revert");
        for(uint256 i;i<beforeState.balances.length;++i) {
            for(uint256 j;j<3;++j) assertEq(afterState.balances[i][j],beforeState.balances[i][j],"account token balance changed after revert");
        }
        for(uint256 i;i<beforeState.slot0.length;++i) {
            assertEq(afterState.slot0[i],beforeState.slot0[i],"V3 slot0 changed after revert");
            assertEq(afterState.liquidity[i],beforeState.liquidity[i],"V3 liquidity changed after revert");
        }
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function test_health_factor_equality_is_not_liquidatable_before_maturity() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.55e18);
        assertEq(pool.getHealthFactor(markets[0],borrower_A),1e18);
        assertFalse(pool.isLiquidatable(markets[0],borrower_A));
        vm.expectRevert(NotLiquidatable.selector);
        _execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function test_stale_price_equality_reverts_then_refresh_allows_same_liquidation() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        uint256 updatedAt=priceOracle.updatedAt();
        vm.warp(updatedAt+1 days);
        vm.expectRevert(abi.encodeWithSelector(StaleOraclePrice.selector,updatedAt));
        _execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.allowance(address(executor),address(pool)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        priceOracle.touch();
        (uint128 repaid,,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,110e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
    }

    function test_healthy_maturity_grace_boundary_needs_fresh_oracle_and_does_not_need_new_debt() public {
        _openProvided(borrower_A);
        vm.warp(uint256(markets[0])+1 days-1);
        priceOracle.touch();
        assertFalse(pool.isLiquidatable(markets[0],borrower_A));
        vm.expectRevert(NotLiquidatable.selector);
        _execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        vm.warp(uint256(markets[0])+1 days);
        assertTrue(pool.isLiquidatable(markets[0],borrower_A));
        (uint128 repaid,,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,110e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
    }

    function test_maturity_does_not_bypass_stale_oracle() public {
        _openProvided(borrower_A);
        uint256 updatedAt=priceOracle.updatedAt();
        vm.warp(uint256(markets[0])+1 days);
        vm.expectRevert(abi.encodeWithSelector(StaleOraclePrice.selector,updatedAt));
        _execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),0);
    }

    function test_discount_slippage_guard_reverts_before_swap_and_can_be_retried() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        LiquidationExecutor.Request memory request=_request(address(pool),borrower_A,110e18,_path(3000),1);
        request.minDiscountOut=0.2e18+1;
        vm.expectRevert(abi.encodeWithSelector(LiquidationDiscountTooLow.selector,0.2e18,0.2e18+1));
        _execute(request);
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),0);
        request.minDiscountOut=0.2e18;
        (uint128 repaid,,,)=_execute(request);
        assertEq(repaid,110e18);
    }

    function test_second_liquidation_of_closed_position_preserves_profit_and_old_inventory() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        vm.prank(address(executor)); asset.mint(77e18);
        vm.prank(address(executor)); collateral.mint(33e18);
        LiquidationExecutor.Request memory request=_request(address(pool),borrower_A,110e18,_path(3000),1);
        (,,,uint256 profit)=_execute(request);
        vm.expectRevert(NotLiquidatable.selector);
        _execute(request);
        assertEq(asset.balanceOf(address(executor)),77e18+profit);
        assertEq(collateral.balanceOf(address(executor)),33e18);
        assertEq(asset.allowance(address(executor),address(pool)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function test_fuzz_real_partial_liquidation_conserves_debt_inventory_and_repayment(
        uint128 requestedSeed,uint96 oldAssetSeed,uint96 oldCollateralSeed
    ) public {
        uint128 requested=uint128(bound(requestedSeed,1e18,100e18));
        uint256 oldAsset=bound(oldAssetSeed,0,1_000e18);
        uint256 oldCollateral=bound(oldCollateralSeed,0,1_000e18);
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        vm.prank(address(executor)); asset.mint(oldAsset);
        vm.prank(address(executor)); collateral.mint(oldCollateral);
        uint256 poolAssetBefore=asset.balanceOf(address(pool));
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,requested,_path(3000),1));
        assertEq(repaid,requested);
        assertEq(uint256(repaid)+pool.debtOf(markets[0],borrower_A),110e18);
        assertEq(uint256(seized)+pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(spent,seized);
        assertGt(profit,0);
        assertEq(asset.balanceOf(address(pool)),poolAssetBefore+repaid);
        assertEq(asset.balanceOf(address(executor)),oldAsset+profit);
        assertEq(collateral.balanceOf(address(executor)),oldCollateral);
        assertEq(asset.allowance(address(executor),address(pool)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function test_provided_debt_full_liquidation_preserves_old_balances_and_exact_profit_equality() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.4e18);
        vm.prank(address(executor)); asset.mint(77e18);
        vm.prank(address(executor)); collateral.mint(33e18);
        uint128 seized = 343.75e18;
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_path(3000),seized);
        uint256 expectedProfit=out-110e18;
        vm.expectEmit(true,true,true,true,address(executor));
        emit LiquidationExecutor.LiquidationExecuted(address(pool),borrower_A,markets[0],
            keccak256(abi.encode(address(pool),borrower_A,uint128(110e18),_path(3000),expectedProfit)),110e18,seized,seized,expectedProfit);
        (uint128 repaid,uint128 actualSeized,uint256 spent,uint256 profit) = _execute(_request(address(pool),borrower_A,110e18,_path(3000),expectedProfit));
        assertEq(repaid,110e18);assertEq(actualSeized,seized);assertEq(spent,seized);assertEq(profit,expectedProfit);
        assertEq(asset.balanceOf(address(executor)),77e18+expectedProfit);
        assertEq(collateral.balanceOf(address(executor)),33e18);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
        assertEq(asset.allowance(address(executor),address(pool)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function test_real_protocol_requested_full_is_capped_and_bad_debt_uses_actual_callback() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.1e18);
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,32e18);assertEq(seized,400e18);assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);assertEq(pool.collateralOf(markets[0],borrower_A),0);
        assertEq(pool.getMarketBalance(markets[0]),32e18);
    }

    function test_real_protocol_partial_and_min_debt_remaining_boundary() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        vm.expectPartialRevert(InsufficientLiquidationAmount.selector);
        _execute(_request(address(pool),borrower_A,101e18,_path(3000),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
        (uint128 repaid,uint128 seized,,)=_execute(_request(address(pool),borrower_A,100e18,_path(3000),1));
        assertEq(repaid,100e18);assertEq(seized,312.5e18);assertEq(pool.debtOf(markets[0],borrower_A),10e18);
    }

    function test_max_recovery_equality_rejects_unrecoverable_raw_collateral_remainder() public {
        _openProvided(borrower_A);
        _deposit_collateral(borrower_A,markets[0],1);
        priceOracle.setPriceFraction(3,50);
        AtomicSnapshot memory beforeState=_atomicSnapshot(borrower_A);
        vm.expectRevert(abi.encodeWithSelector(InsufficientLiquidationAmount.selector,90.8e18,0,10e18));
        _execute(_request(address(pool),borrower_A,19.2e18,_path(3000),1));
        _assertAtomicSnapshot(beforeState,borrower_A);
        (uint128 repaid,uint128 seized,,)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),1));
        assertEq(repaid,19.2e18);assertEq(seized,400e18+1);
        assertEq(pool.collateralOf(markets[0],borrower_A),0);
        assertEq(pool.debtOf(markets[0],borrower_A),0);
    }

    function test_partial_requires_recoverable_collateral_remainder_and_accepts_exact_boundary() public {
        _openProvided(borrower_A);
        priceOracle.setPrice(0.1e18);
        AtomicSnapshot memory beforeState=_atomicSnapshot(borrower_A);
        vm.expectRevert(abi.encodeWithSelector(InsufficientLiquidationAmount.selector,88e18-1,12.5e18-2,10e18));
        _execute(_request(address(pool),borrower_A,22e18+1,_path(3000),1));
        _assertAtomicSnapshot(beforeState,borrower_A);
        (uint128 repaid,uint128 seized,,uint256 profit)=_execute(_request(address(pool),borrower_A,22e18,_path(3000),1));
        assertEq(repaid,22e18);
        assertEq(seized,275e18);
        assertGt(profit,0);
        assertEq(pool.debtOf(markets[0],borrower_A),88e18);
        assertEq(pool.collateralOf(markets[0],borrower_A),125e18);
    }

    function test_real_v3_reverse_orientation_executes_same_atomic_flow() public {
        (MockCreditPool reverse,MockPriceOracle oracle)=_newPool(collateral,asset);
        _fundPool(reverse,collateral,asset,borrower_A);oracle.setPrice(0.4e18);
        bytes memory path=abi.encodePacked(address(asset),uint24(3000),address(collateral));
        (uint128 repaid,,,uint256 profit)=_execute(_request(address(reverse),borrower_A,100e18,path,1));
        assertEq(repaid,100e18);assertGt(profit,0);assertEq(reverse.debtOf(markets[0],borrower_A),0);
    }

    function test_real_same_token_maturity_uses_one_baseline_and_no_router_approval() public {
        (MockCreditPool same,MockPriceOracle oracle)=_newPool(asset,asset);
        _fundPool(same,asset,asset,borrower_A);
        vm.warp(uint256(markets[0])+1 days);oracle.touch();
        vm.prank(address(executor));asset.mint(77e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(same),borrower_A,100e18,"",1));
        assertEq(repaid,100e18);assertEq(spent,0);assertEq(profit,uint256(seized)-100e18);
        assertEq(asset.balanceOf(address(executor)),77e18+profit);assertEq(asset.allowance(address(executor),router),0);
    }

    function test_real_two_hop_quote_execution_and_old_middle_inventory() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        wrapped.transfer(address(executor),77e18);
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_twoHop(3000,3000),343.75e18);
        (,,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_twoHop(3000,3000),out-110e18));
        assertEq(profit,out-110e18);assertEq(wrapped.balanceOf(address(executor)),77e18);
        assertEq(wrapped.balanceOf(router),0);
    }

    function test_real_first_hop_partial_input_crosses_tick_preserves_residual() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.1e18);
        vm.prank(address(executor));collateral.mint(77e18);
        (uint256 out,,uint32[] memory ticks,) = IFixtureQuoter(quoter).quoteExactInput(_path(10000),400e18);
        assertGt(ticks[0],0);assertGt(out,32e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(10000),out-32e18));
        assertEq(repaid,32e18);assertEq(seized,400e18);assertLt(spent,seized);assertGt(spent,0);
        assertEq(profit,out-32e18);assertEq(collateral.balanceOf(address(executor)),77e18+400e18-spent);
        assertEq(IFixtureV3Pool(thinDirect).liquidity(),0);
    }

    function test_real_two_hop_first_partial_input_still_profitable() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.1e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_twoHop(10000,3000),1));
        assertEq(repaid,32e18);assertEq(seized,400e18);assertLt(spent,seized);assertGt(profit,0);
        assertEq(wrapped.balanceOf(router),0);
    }

    function test_real_two_hop_second_partial_retains_intermediate_at_executor() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.1e18);
        wrapped.transfer(address(executor),77e18);wrapped.transfer(router,11e18);
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_twoHop(3000,10000),400e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_twoHop(3000,10000),out-32e18));
        assertEq(repaid,32e18);assertEq(seized,400e18);assertEq(spent,400e18);
        assertEq(profit,out-32e18);assertEq(wrapped.balanceOf(router),11e18);
        assertGt(wrapped.balanceOf(address(executor)),77e18);
        assertEq(asset.balanceOf(address(executor)),profit);
    }

    function test_real_depleted_route_reverts_without_position_or_inventory_change() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.1e18);
        _execute(_request(address(pool),borrower_A,110e18,_path(10000),1));
        priceOracle.setPrice(1e18);_openProvided(borrower_B);priceOracle.setPrice(0.1e18);
        wrapped.transfer(address(executor),11e18);
        AtomicSnapshot memory beforeState=_atomicSnapshot(borrower_B);
        vm.expectRevert();_execute(_request(address(pool),borrower_B,110e18,_path(10000),1));
        _assertAtomicSnapshot(beforeState,borrower_B);
        assertEq(pool.debtOf(markets[0],borrower_B),110e18);assertEq(pool.collateralOf(markets[0],borrower_B),400e18);
    }

    function test_guard_one_above_quote_reverts_entire_protocol_and_swap() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        vm.prank(address(executor)); asset.mint(77e18);
        vm.prank(address(executor)); collateral.mint(33e18);
        wrapped.transfer(address(executor),11e18);
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_path(3000),343.75e18);
        AtomicSnapshot memory beforeState=_atomicSnapshot(borrower_A);
        vm.expectRevert();_execute(_request(address(pool),borrower_A,110e18,_path(3000),out-110e18+1));
        _assertAtomicSnapshot(beforeState,borrower_A);
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),77e18);assertEq(collateral.allowance(address(executor),router),0);
    }

    function test_two_hop_guard_revert_restores_both_pools_and_preexisting_router_inventory() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        vm.prank(address(executor)); asset.mint(77e18);
        vm.prank(address(executor)); collateral.mint(33e18);
        wrapped.transfer(address(executor),11e18);
        wrapped.transfer(router,7e18);
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_twoHop(3000,3000),343.75e18);
        AtomicSnapshot memory beforeState=_atomicSnapshot(borrower_A);
        vm.expectRevert();_execute(_request(address(pool),borrower_A,110e18,_twoHop(3000,3000),out-110e18+1));
        _assertAtomicSnapshot(beforeState,borrower_A);
        assertEq(wrapped.balanceOf(router),7e18);
        assertEq(wrapped.balanceOf(address(executor)),11e18);
    }

    function test_router_native_donation_never_subsidizes_liquidation() public {
        new ForceNative{value:1e18}(router);
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        (uint256 out,,,) = IFixtureQuoter(quoter).quoteExactInput(_path(3000),343.75e18);
        (,,,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_path(3000),out-110e18));
        assertEq(profit,out-110e18);assertEq(router.balance,1e18);
    }

    function test_deadline_and_unsupported_route_fail_before_transfer() public {
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
        LiquidationExecutor.Request memory request=_request(address(pool),borrower_A,110e18,_path(3000),1);
        request.deadline=0;vm.expectRevert(LiquidationExecutor.Expired.selector);_execute(request);
        request.deadline=block.timestamp;request.path=bytes.concat(_path(3000),hex"00");
        vm.expectRevert(LiquidationExecutor.InvalidRoute.selector);_execute(request);
        request.path=_twoHop(3000,3000);vm.prank(fixtureOwner);executor.setIntermediate(address(wrapped),false);
        vm.expectRevert(LiquidationExecutor.InvalidRoute.selector);_execute(request);
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
    }
}
