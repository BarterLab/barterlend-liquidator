// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ExecutorFixture, MockCreditPool, MockPriceOracle, TestLendingFactory } from "./ExecutorFixture.sol";
import { AdversarialCreditPool } from "./ExecutorSecurity.t.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";

contract ExecutorFactoryTest is ExecutorFixture {
    function test_later_factory_pool_liquidates_without_executor_registration() public {
        (MockCreditPool fresh, MockPriceOracle oracle) = _newPool(asset,collateral);
        assertTrue(executor.isPool(address(fresh)));
        (address a,address c,bool recognized) = executor.poolConfig(address(fresh));
        assertTrue(recognized); assertEq(a,address(asset)); assertEq(c,address(collateral));
        _fundPool(fresh,asset,collateral,borrower_A);
        oracle.setPrice(UNIT/3);
        (uint128 repaid,,,uint256 profit) = _execute(_request(address(fresh),borrower_A,100e18,_path(3000),1));
        assertEq(repaid,100e18); assertGt(profit,0); assertEq(fresh.debtOf(markets[0],borrower_A),0);
        assertEq(asset.allowance(address(executor),address(fresh)),0);
    }

    function test_lookalike_and_other_factory_pools_cannot_spend_old_balance() public {
        AdversarialCreditPool outsider = new AdversarialCreditPool(address(asset),address(asset));
        vm.prank(address(executor)); asset.mint(77e18);
        assertFalse(executor.isPool(address(outsider)));
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        _execute(_request(address(outsider),borrower_A,100e18,"",1));
        TestLendingFactory other = new TestLendingFactory(address(this));
        other.registerPool(address(outsider));
        assertFalse(executor.isPool(address(outsider)));
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        _execute(_request(address(outsider),borrower_A,100e18,"",1));
        assertEq(asset.balanceOf(address(executor)),77e18);
        assertEq(asset.allowance(address(executor),address(outsider)),0);
    }

    function test_factory_code_change_fails_closed() public {
        vm.etch(address(lendingFactory),hex"600160005260206000f3"); // Answers true, but no longer has the pinned code.
        assertFalse(executor.isPool(address(pool)));
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        _execute(_request(address(pool),borrower_A,100e18,_path(3000),1));
    }

    function test_factory_revert_fails_closed_and_other_trusted_factory_still_works() public {
        TestLendingFactory second = new TestLendingFactory(address(this));
        second.registerPool(address(pool));
        address[] memory trusted = new address[](2); trusted[0]=address(lendingFactory); trusted[1]=address(second);
        LiquidationExecutor target = new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,trusted);
        vm.mockCallRevert(address(lendingFactory),abi.encodeWithSignature("isPool(address)",address(pool)),hex"deadbeef");
        assertFalse(executor.isPool(address(pool))); assertTrue(target.isPool(address(pool)));
        assertEq(target.lendingFactories().length,2);
        assertEq(target.lendingFactoryCodeHashes(address(second)),address(second).codehash);
    }

    function test_registered_wrong_pool_version_and_no_code_are_rejected() public {
        vm.mockCall(address(pool),abi.encodeWithSignature("version()"),abi.encode(uint256(6)));
        (,,bool recognized)=executor.poolConfig(address(pool)); assertFalse(recognized);
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        _execute(_request(address(pool),borrower_A,100e18,_path(3000),1));
        assertFalse(executor.isPool(address(0))); assertFalse(executor.isPool(borrower_A));
    }

    function test_factory_constructor_requires_bounded_distinct_versioned_contracts() public {
        address[] memory trusted = new address[](0);
        vm.expectRevert(LiquidationExecutor.InvalidAddress.selector);
        new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,trusted);
        trusted=new address[](9);
        vm.expectRevert(LiquidationExecutor.InvalidAddress.selector);
        new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,trusted);
        trusted=new address[](1);
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,trusted);
        trusted=new address[](2); trusted[0]=address(lendingFactory);trusted[1]=address(lendingFactory);
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,trusted);
        vm.mockCall(address(lendingFactory),abi.encodeWithSignature("poolVersion()"),abi.encode(uint256(6)));
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,_lendingFactories());
    }

    function test_old_set_pool_selector_does_not_exist_even_for_owner() public {
        vm.prank(fixtureOwner);
        (bool ok,) = address(executor).call(abi.encodeWithSignature("setPool(address,address,address,bool)",address(pool),address(asset),address(collateral),true));
        assertFalse(ok);
    }
}
