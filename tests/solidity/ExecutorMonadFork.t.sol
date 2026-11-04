// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;
import {Test} from "forge-std/Test.sol";
import {stdStorage, StdStorage} from "forge-std/StdStorage.sol";
import {IERC20} from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import {ICreditPool} from "src/interfaces/ICreditPool.sol";
import {IPriceOracle} from "src/interfaces/IPriceOracle.sol";
import {LiquidationExecutor} from "../../contracts/LiquidationExecutor.sol";
import {IExecutorV4} from "../../contracts/IExecutorV4.sol";
interface IForkV4Quoter {
    struct PathKey {address intermediateCurrency;uint24 fee;int24 tickSpacing;address hooks;bytes hookData;}
    struct Params {address exactCurrency;PathKey[] path;uint128 exactAmount;}
    function quoteExactInput(Params calldata params) external returns(uint256,uint256);
}

/// @dev Explicit opt-in. All writes occur in local EVM memory. Existing mainnet
/// tokens, lending, adapters, pools and V4 code are exercised at the pinned block.
/// Oracle return overrides are test-only fault injection to control eligibility.
contract ExecutorMonadFork is Test {
    using stdStorage for StdStorage;
    address constant WETH=address(bytes20(hex"ee8c0e9f1bffb4eb878d8f15f368a02a35481242"));
    address constant AUSD=address(bytes20(hex"00000000efe302beaa2b3e6e1b18d08d69a9012a"));
    address constant USDC=address(bytes20(hex"754704bc059f8c67012fed69bc8a327a5aafb603"));
    address constant MANAGER=address(bytes20(hex"188d586ddcf52439676ca21a244753fa19f9ea8e"));
    address constant QUOTER=address(bytes20(hex"a222dd357a9076d1091ed6aa2e16c9742dd26891"));
    address constant ROUTER3=address(bytes20(hex"fe31f71c1b106eac32f1a19239c9a9a72ddfb900"));
    address constant FACTORY3=address(bytes20(hex"204faca1764b154221e35c0d20abb3c525710498"));
    ICreditPool internal pool=ICreditPool(address(bytes20(hex"5fe2ff5ace915d525a56955a54a3b8c0bc6eb8d5")));
    LiquidationExecutor internal executor;
    address internal borrower=address(0xB0110);
    address internal lender=address(0x1E11D);
    uint64 internal market;
    IPriceOracle.Price internal originalPrice;
    function setUp() public {
        if(bytes(vm.envOr("BARTERLEND_MONAD_FORK_URL",string(""))).length==0){vm.skip(true);return;}
        require(block.chainid==31337 && block.number==117583907,"explicit pinned local fork required");
        assertEq(MANAGER.codehash,0xf5d7a45765367807b101f825ec74004078c79bd96c621e481c608ee22f7336d4);
        assertEq(QUOTER.codehash,0x3666b10d21aee597139d129b378b2cdaeaaaaaffeb142c1f64366a7bd5be8b3b);
        assertEq(pool.asset(),AUSD);assertEq(pool.collateral(),WETH);assertEq(pool.version(),5);
        originalPrice=IPriceOracle(pool.priceOracle()).getPrice();
        market=pool.getActiveMarkets()[0];
        address[] memory lendingFactories=new address[](1);
        lendingFactories[0]=address(bytes20(hex"dd993ff5d323499388ec316d1286bde639c76670"));
        executor=new LiquidationExecutor(address(this),address(this),ROUTER3,FACTORY3,lendingFactories);
        executor.setVerifiedToken(WETH,true);executor.setVerifiedToken(AUSD,true);executor.setVerifiedToken(USDC,true);
        executor.setIntermediate(USDC,true);executor.configureV4(MANAGER);
        executor.setV4Pool(IExecutorV4.PoolKey(USDC,WETH,500,10,address(0)),true);
        executor.setV4Pool(IExecutorV4.PoolKey(AUSD,USDC,50,1,address(0)),true);
        assertTrue(executor.isPool(address(pool)));
        deal(WETH,borrower,0.1e18);
        // AUSD stores balance in a packed slot; preserve its adjacent flags.
        stdstore.target(AUSD).sig(IERC20.balanceOf.selector).with_key(lender).enable_packed_slots().checked_write(uint256(101e6));
        assertEq(IERC20(AUSD).balanceOf(lender),101e6);
        vm.startPrank(borrower);IERC20(WETH).approve(address(pool),0.1e18);pool.depositCollateral(borrower,market,0.1e18);vm.stopPrank();
        vm.startPrank(lender);IERC20(AUSD).approve(address(pool),101e6);pool.deposit(lender,101e6);pool.placeProvideOrder(lender,market,0,101e6);vm.stopPrank();
        vm.prank(borrower);pool.borrow(borrower,market,100e6,110e6);
        assertGt(pool.debtOf(market,borrower),0);assertFalse(pool.isLiquidatable(market,borrower));
    }
    function _path() internal pure returns(bytes memory) {
        return abi.encodePacked(bytes1(0x04),WETH,uint24(500),int24(10),USDC,uint24(50),int24(1),AUSD);
    }
    function _request() internal view returns(LiquidationExecutor.Request memory) {
        return LiquidationExecutor.Request(address(pool),market,borrower,pool.debtOf(market,borrower),0,block.timestamp+60,_path(),1,1,keccak256("pinned monad fork"));
    }
    function _unhealthy() internal {
        vm.mockCall(pool.priceOracle(),abi.encodeCall(IPriceOracle.getPrice,()),abi.encode(IPriceOracle.Price(1100e6,1e18,block.timestamp)));
        assertLt(pool.getHealthFactor(market,borrower),1e18);assertTrue(pool.isLiquidatable(market,borrower));
    }
    function _quote(uint128 seized) internal returns(uint256 out) {
        IForkV4Quoter.PathKey[] memory path=new IForkV4Quoter.PathKey[](2);
        path[0]=IForkV4Quoter.PathKey(USDC,500,10,address(0),"");path[1]=IForkV4Quoter.PathKey(AUSD,50,1,address(0),"");
        (out,)=IForkV4Quoter(QUOTER).quoteExactInput(IForkV4Quoter.Params(WETH,path,seized));
    }
    function _executeAndCheck() internal {
        LiquidationExecutor.Request memory req=_request();
        uint256 snapshot=vm.snapshotState();
        (,uint128 predictedSeized,,)=executor.execute(req);
        assertTrue(vm.revertToStateAndDelete(snapshot));
        uint256 quoted=_quote(predictedSeized);
        req.minSwapOutput=quoted*99/100;
        uint256 middle=IERC20(USDC).balanceOf(address(executor));
        uint256 repayBefore=IERC20(AUSD).balanceOf(address(pool));
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=executor.execute(req);
        assertEq(repaid,req.requestedDebt);assertEq(seized,predictedSeized);assertEq(spent,seized);
        assertEq(profit+repaid,quoted);assertGt(profit,0);assertEq(pool.debtOf(market,borrower),0);
        assertEq(IERC20(AUSD).balanceOf(address(pool)),repayBefore+repaid);
        assertEq(IERC20(AUSD).balanceOf(address(executor)),profit);assertEq(IERC20(USDC).balanceOf(address(executor)),middle);
        assertEq(IERC20(AUSD).allowance(address(executor),address(pool)),0);assertEq(uint256(executor.phase()),0);
        emit log_named_uint("V4 actual output AUSD raw",quoted);emit log_named_uint("V4 asset profit raw",profit);
    }
    function test_fork_hf_liquidation_real_lending_and_v4_pools() public {_unhealthy();_executeAndCheck();}
    function test_fork_maturity_healthy_hf_real_lending_and_v4_pools() public {
        vm.warp(uint256(market)+pool.getLiquidationGracePeriod(market));
        originalPrice.updatedAt=block.timestamp;
        vm.mockCall(pool.priceOracle(),abi.encodeCall(IPriceOracle.getPrice,()),abi.encode(originalPrice));
        assertGe(pool.getHealthFactor(market,borrower),1e18);assertTrue(pool.isLiquidatable(market,borrower));_executeAndCheck();
    }
    function test_fork_quote_floor_failure_rolls_back_lending_and_real_pools() public {
        _unhealthy();LiquidationExecutor.Request memory req=_request();req.minSwapOutput=10000e6;
        uint256 managerWeth=IERC20(WETH).balanceOf(MANAGER);uint256 managerAusd=IERC20(AUSD).balanceOf(MANAGER);
        vm.expectRevert(LiquidationExecutor.InsufficientProfit.selector);executor.execute(req);
        assertEq(pool.debtOf(market,borrower),req.requestedDebt);assertEq(pool.collateralOf(market,borrower),0.1e18);
        assertEq(IERC20(WETH).balanceOf(MANAGER),managerWeth);assertEq(IERC20(AUSD).balanceOf(MANAGER),managerAusd);
        assertEq(IERC20(AUSD).balanceOf(address(executor)),0);
    }
}
