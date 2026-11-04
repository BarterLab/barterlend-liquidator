// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;
import { ExecutorFixture } from "./ExecutorFixture.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";
import { IExecutorV4 } from "../../contracts/IExecutorV4.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

// Deterministic fault injection. Genuine PoolManager execution is covered by
// ExecutorMonadFork; this double checks every callback and settlement boundary.
contract FaultV4Manager is IExecutorV4 {
    uint8 public mode;
    uint256 public firstBps=10000;
    uint256 public secondBps=10000;
    uint256 private hops;
    address private synced;
    uint256 private balanceBefore;
    address[] private currencies;
    mapping(address => int256) public delta;
    function configure(uint8 m,uint256 first,uint256 second) external { mode=m;firstBps=first;secondBps=second; }
    function unlock(bytes calldata data) external returns (bytes memory) {
        if(mode==1)return "";
        hops=0;
        bytes memory result=LiquidationExecutor(msg.sender).unlockCallback(mode==2?abi.encode(bytes32(0)):data);
        if(mode==3)LiquidationExecutor(msg.sender).unlockCallback(data);
        for(uint256 i;i<currencies.length;i++) require(delta[currencies[i]]==0,"unsettled");
        delete currencies;
        return result;
    }
    function swap(PoolKey calldata key,SwapParams calldata params,bytes calldata) external returns(int256) {
        uint256 amount=uint256(-params.amountSpecified);
        uint256 spent=amount*(hops++==0?firstBps:secondBps)/10000;
        if(mode==5)spent=amount+1;
        address input=params.zeroForOne?key.currency0:key.currency1;
        address output=params.zeroForOne?key.currency1:key.currency0;
        delta[input]-=int256(spent);delta[output]+=int256(spent);
        currencies.push(input);currencies.push(output);
        int128 debit=mode==4?int128(uint128(spent)):-int128(uint128(spent));
        int128 credit=int128(uint128(spent));
        int128 d0=params.zeroForOne?debit:credit;
        int128 d1=params.zeroForOne?credit:debit;
        return (int256(d0)<<128)|int256(uint256(uint128(d1)));
    }
    function sync(address currency) external {synced=currency;balanceBefore=IERC20(currency).balanceOf(address(this));}
    function settle() external payable returns(uint256 paid) {
        paid=IERC20(synced).balanceOf(address(this))-balanceBefore;
        delta[synced]+=int256(paid);
        if(mode==6)paid++;
    }
    function take(address currency,address to,uint256 amount) external {
        require(delta[currency]>=int256(amount));delta[currency]-=int256(amount);
        IERC20(currency).transfer(to,mode==7?amount/2:amount);
    }
}
contract ExecutorV4Test is ExecutorFixture {
    FaultV4Manager internal manager;
    function beforeEach() public override {
        super.beforeEach();
        manager=new FaultV4Manager();
        vm.prank(fixtureOwner);executor.configureV4(address(manager));
        asset.transfer(address(manager),100000e18);wrapped.transfer(address(manager),100000e18);
        _allow(address(collateral),address(asset),500,10);
        _allow(address(collateral),address(wrapped),500,10);
        _allow(address(wrapped),address(asset),50,1);
        _openProvided(borrower_A);priceOracle.setPrice(0.4e18);
    }
    function _key(address a,address b,uint24 fee,int24 spacing) internal pure returns(IExecutorV4.PoolKey memory) {
        return IExecutorV4.PoolKey(a<b?a:b,a<b?b:a,fee,spacing,address(0));
    }
    function _allow(address a,address b,uint24 fee,int24 spacing) internal {
        IExecutorV4.PoolKey memory key=_key(a,b,fee,spacing);
        vm.prank(fixtureOwner);executor.setV4Pool(key,true);
    }
    function _v4Path(bool two) internal view returns(bytes memory) {
        if(!two)return abi.encodePacked(bytes1(0x04),address(collateral),uint24(500),int24(10),address(asset));
        return abi.encodePacked(bytes1(0x04),address(collateral),uint24(500),int24(10),address(wrapped),uint24(50),int24(1),address(asset));
    }
    function test_v4_direct_and_exact_quote_floor() public {
        LiquidationExecutor.Request memory req=_request(address(pool),borrower_A,110e18,_v4Path(false),1);
        req.minSwapOutput=343.75e18;
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(req);
        assertEq(repaid,110e18);assertEq(seized,343.75e18);assertEq(spent,seized);assertEq(profit,233.75e18);
        assertEq(pool.debtOf(markets[0],borrower_A),0);assertEq(collateral.allowance(address(executor),address(manager)),0);
    }
    function test_v4_two_hop_partial_second_keeps_all_intermediate_and_old_inventory() public {
        manager.configure(0,10000,5000);
        wrapped.transfer(address(executor),77e18);wrapped.transfer(router,11e18);
        collateral.transfer(address(executor),33e18);asset.transfer(address(executor),99e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_v4Path(true),1));
        assertEq(repaid,110e18);assertEq(spent,seized);assertEq(profit,61.875e18);
        assertEq(wrapped.balanceOf(address(executor)),77e18+171.875e18);assertEq(wrapped.balanceOf(router),11e18);
        assertEq(collateral.balanceOf(address(executor)),33e18);assertEq(asset.balanceOf(address(executor)),99e18+profit);
        assertEq(manager.delta(address(wrapped)),0);
        vm.prank(fixtureOwner);executor.withdraw(address(wrapped),fixtureOwner,171.875e18);
        assertEq(wrapped.balanceOf(address(executor)),77e18);
    }
    function test_v4_partial_first_keeps_unspent_collateral() public {
        manager.configure(0,5000,10000);
        (,,uint256 spent,uint256 profit)=_execute(_request(address(pool),borrower_A,110e18,_v4Path(true),1));
        assertEq(spent,171.875e18);assertEq(profit,61.875e18);assertEq(collateral.balanceOf(address(executor)),171.875e18);
    }
    function test_v4_quote_floor_reverts_even_when_repayment_and_gas_are_covered() public {
        LiquidationExecutor.Request memory req=_request(address(pool),borrower_A,110e18,_v4Path(true),1);
        req.minSwapOutput=343.75e18+1;
        vm.expectRevert(LiquidationExecutor.InsufficientProfit.selector);_execute(req);
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),0);assertEq(manager.delta(address(collateral)),0);
    }
    function testFuzz_v4_callback_or_settlement_fault_is_atomic(uint8 fault) public {
        fault=uint8(bound(fault,1,6));manager.configure(fault,10000,10000);
        vm.expectRevert();_execute(_request(address(pool),borrower_A,110e18,_v4Path(true),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);assertEq(pool.collateralOf(markets[0],borrower_A),400e18);
        assertEq(asset.balanceOf(address(executor)),0);assertEq(uint256(executor.phase()),0);
    }
    function test_v4_inexact_intermediate_transfer_reverts() public {
        manager.configure(7,10000,5000);
        vm.expectRevert(LiquidationExecutor.CallbackAmountsMismatch.selector);_execute(_request(address(pool),borrower_A,110e18,_v4Path(true),1));
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
    }
    function test_v4_callback_only_during_own_unlock() public {
        vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);executor.unlockCallback(abi.encode(bytes32(0)));
        vm.prank(address(manager));vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);executor.unlockCallback(abi.encode(bytes32(0)));
    }
    function test_v4_manager_immutable_and_owner_only() public {
        vm.expectRevert(LiquidationExecutor.Unauthorized.selector);executor.configureV4(address(manager));
        vm.prank(fixtureOwner);vm.expectRevert(LiquidationExecutor.InvalidAddress.selector);executor.configureV4(address(manager));
    }
    function test_v4_unapproved_pool_and_hook_are_rejected() public {
        IExecutorV4.PoolKey memory key=_key(address(collateral),address(asset),500,10);
        vm.prank(fixtureOwner);executor.setV4Pool(key,false);
        vm.expectRevert(LiquidationExecutor.InvalidRoute.selector);_execute(_request(address(pool),borrower_A,110e18,_v4Path(false),1));
        key.hooks=address(1);vm.prank(fixtureOwner);vm.expectRevert(LiquidationExecutor.InvalidRoute.selector);executor.setV4Pool(key,true);
    }
    function test_v3_quote_floor_is_enforced_independently_of_profit() public {
        LiquidationExecutor.Request memory req=_request(address(pool),borrower_A,110e18,_path(3000),1);
        req.minSwapOutput=1000e18;
        vm.expectRevert();_execute(req);
        assertEq(pool.debtOf(markets[0],borrower_A),110e18);
    }
}
