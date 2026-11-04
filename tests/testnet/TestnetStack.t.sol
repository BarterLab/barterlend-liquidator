// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { Test } from "forge-std/Test.sol";
import { CreditPool } from "src/CreditPool.sol";
import { CreditPoolFacet1 } from "src/facets/CreditPoolFacet1.sol";
import { CreditPoolFacet2 } from "src/facets/CreditPoolFacet2.sol";
import { CreditPoolFacet3 } from "src/facets/CreditPoolFacet3.sol";
import { CreditPoolFacet4 } from "src/facets/CreditPoolFacet4.sol";
import { AaveV3Adapter } from "src/adapters/AaveV3Adapter.sol";
import { ICreditPoolTypes, ICreditPoolErrors } from "src/interfaces/ICreditPool.sol";
import { UnderlyingPoolConfig } from "src/interfaces/IUnderlyingPool.sol";
import { MarketsConfigParams } from "src/lib/MarketsConfig.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import { TestLendingFactory } from "../fixtures/factory/TestLendingFactory.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";
import { TestnetToken, TestnetWrappedMON, TestnetOracle, TestnetAaveReceipt, TestnetAavePool, TestnetV3Bootstrap, ITestnetV3Factory, ITestnetV3Pool } from "./TestnetDependencies.sol";

interface ITestnetQuoter {
    function quoteExactInput(bytes calldata path, uint256 amount) external returns (uint256, uint160[] memory, uint32[] memory, uint256);
    function quoteExactOutput(bytes calldata path, uint256 amount) external returns (uint256, uint160[] memory, uint32[] memory, uint256);
}
interface ITestnetPermit2 { function DOMAIN_SEPARATOR() external view returns (bytes32); }

/// @notice Deployable dependencies use no cheatcodes. Only the fixture controls local funding, time and actors.
contract TestnetStackTest is Test {
    uint160 private constant Q96 = 79228162514264337593543950336;
    uint24 private constant FEE = 3000;
    int24 private constant LOWER = -887220;
    int24 private constant UPPER = 887220;
    TestnetToken private asset;
    TestnetToken private collateral;
    TestnetWrappedMON private wrapped;
    TestnetOracle private oracle;
    TestnetAaveReceipt private receipt;
    TestnetAavePool private underlying;
    AaveV3Adapter private adapter;
    TestnetV3Bootstrap private bootstrap;
    CreditPool private pool;
    LiquidationExecutor private executor;
    address private factory;
    address private router;
    address private quoter;
    address private permit2;
    address private directPool;
    address private nativePool;
    address private provider = address(0x1001);
    address private borrower = address(0x1002);
    address private operator = address(0x1003);
    uint64 private market;

    function setUp() public {
        vm.chainId(10143);
        vm.warp(1_800_000_000);
        vm.deal(address(this), 2 ether);
        asset = new TestnetToken(address(this), "Test Stack Asset", "tsASSET");
        collateral = new TestnetToken(address(this), "Test Stack Collateral", "tsCOLL");
        wrapped = new TestnetWrappedMON();
        oracle = new TestnetOracle(address(this), address(collateral), address(asset));
        receipt = new TestnetAaveReceipt(address(this), address(asset));
        underlying = new TestnetAavePool(address(asset), receipt);
        receipt.setPool(address(underlying));
        adapter = new AaveV3Adapter();
        permit2 = _deploy(vm.envBytes("BL_TESTNET_PERMIT_CREATION"), "");
        factory = _artifact("UniswapV3Factory", "");
        router = _artifact("SwapRouter02", abi.encode(address(0), factory, address(0), address(wrapped)));
        quoter = _artifact("QuoterV2", abi.encode(factory, address(wrapped)));
        bootstrap = new TestnetV3Bootstrap(address(this), factory);
        asset.mint(address(this), 2_000_000e18);
        collateral.mint(address(this), 2_000_000e18);
        wrapped.deposit{value: 1 ether}();
        asset.approve(address(bootstrap), type(uint256).max);
        collateral.approve(address(bootstrap), type(uint256).max);
        wrapped.approve(address(bootstrap), type(uint256).max);
        (directPool,,) = bootstrap.seedPool(address(collateral), address(asset), FEE, Q96, LOWER, UPPER, 1_000_000e18);
        (nativePool,,) = bootstrap.seedPool(address(asset), address(wrapped), FEE, address(asset) < address(wrapped) ? Q96 / 32 : Q96 * 32, LOWER, UPPER, 32e18);
        address[] memory facets = new address[](4);
        facets[0] = address(new CreditPoolFacet1()); facets[1] = address(new CreditPoolFacet2());
        facets[2] = address(new CreditPoolFacet3()); facets[3] = address(new CreditPoolFacet4());
        ICreditPoolTypes.InitializationParams memory params = ICreditPoolTypes.InitializationParams({
            owner: address(this), permit2: permit2, asset: address(asset),
            underlyingPool: UnderlyingPoolConfig(address(adapter), bytes32(uint256(uint160(address(receipt))))),
            priceOracle: address(oracle), collateral: address(collateral), stalePriceWindow: 3600,
            protocolFee: 0, totalCollateralCap: type(uint128).max, provideOrderMinAmount: 1e18,
            borrowOrderMinAmount: 1e18, maxActiveProvideOrders: 8, maxActiveBorrowOrders: 1,
            borrowLTV: 5e17, liquidationLTV: 5e17, liquidationDiscountMin: 5e16,
            liquidationDiscountMax: 2e17, liquidationGracePeriod: 60,
            minDebtRemainingAfterLiquidation: 10e18,
            marketsConfig: MarketsConfigParams([uint64(600), 600, 600], [uint8(3), 3, 3], [uint64(0), 0, 0])
        });
        TestLendingFactory lendingFactory = new TestLendingFactory(address(this));
        pool = CreditPool(lendingFactory.createPool(bytes.concat(type(CreditPool).creationCode, abi.encode(params, facets))));
        market = pool.getActiveMarkets()[0];
        address[] memory lendingFactories = new address[](1); lendingFactories[0] = address(lendingFactory);
        executor = new LiquidationExecutor(address(this), operator, router, factory, lendingFactories);
        executor.setVerifiedToken(address(asset), true); executor.setVerifiedToken(address(collateral), true);
        executor.setVerifiedToken(address(wrapped), true); executor.setFee(FEE, true);
        executor.setIntermediate(address(wrapped), true);
    }

    function test_testnet_permit2_dynamic_domain_matches_chain_and_deployed_address() public view {
        bytes32 expected = keccak256(abi.encode(keccak256("EIP712Domain(string name,uint256 chainId,address verifyingContract)"), keccak256("Permit2"), uint256(10143), permit2));
        assertEq(ITestnetPermit2(permit2).DOMAIN_SEPARATOR(), expected);
    }

    function test_testnet_admin_and_callback_access_restricted() public {
        vm.startPrank(borrower);
        vm.expectRevert(); asset.mint(borrower, 1);
        vm.expectRevert(); oracle.setPriceFraction(1, 1);
        vm.expectRevert(); oracle.setObservation(1, 1, block.timestamp);
        vm.expectRevert(); oracle.touch();
        vm.expectRevert(); receipt.setPool(address(underlying));
        vm.expectRevert(); receipt.mint(borrower, 1);
        vm.expectRevert(); receipt.burn(address(this), 1);
        vm.expectRevert(); bootstrap.seedPool(address(collateral), address(asset), 500, Q96, -887270, 887270, 1e18);
        vm.expectRevert(); bootstrap.removeLiquidity(nativePool, LOWER, UPPER, 1);
        vm.expectRevert(); bootstrap.uniswapV3MintCallback(1, 1, "");
        vm.stopPrank();
        vm.expectRevert(); receipt.setPool(address(underlying));
    }

    function test_testnet_underlying_real_adapter_deposit_withdraw_exact_backing() public {
        asset.mint(provider, 100e18);
        vm.startPrank(provider);
        asset.approve(address(pool), 100e18);
        pool.deposit(provider, 100e18);
        assertEq(pool.balanceOf(provider), 100e18);
        // A plain deposit stays idle in CreditPool; lender orders use the underlying pool.
        assertEq(receipt.balanceOf(address(pool)), 0);
        pool.placeProvideOrder(provider, market, 0, 100e18);
        assertEq(pool.balanceOf(provider), 0);
        assertEq(receipt.balanceOf(address(pool)), 100e18);
        assertEq(asset.balanceOf(address(underlying)), receipt.totalSupply());
        pool.cancelProvideOrder(provider, market, 0, 40e18);
        pool.withdraw(provider, provider, 40e18);
        assertEq(asset.balanceOf(provider), 40e18);
        assertEq(pool.balanceOf(provider), 0);
        assertEq(receipt.balanceOf(address(pool)), 60e18);
        assertEq(asset.balanceOf(address(underlying)), receipt.totalSupply());
        vm.stopPrank();
        vm.expectRevert(); adapter.deposit(address(asset), bytes32(uint256(uint160(address(receipt)))), 1);
    }

    function test_testnet_real_v3_native_cost_quote_and_liquidity_reclaim() public {
        uint256 nativeBefore = wrapped.balanceOf(address(this));
        (uint256 needed,,,) = ITestnetQuoter(quoter).quoteExactOutput(abi.encodePacked(address(wrapped), FEE, address(asset)), 1e15);
        assertGt(needed, 1e18);
        assertLt(needed, 2e18);
        bootstrap.removeLiquidity(nativePool, LOWER, UPPER, 32e18);
        uint256 returned = wrapped.balanceOf(address(this)) - nativeBefore;
        assertGt(returned, 999e15);
        wrapped.withdraw(returned);
        assertGe(address(this).balance, 1.999 ether);
    }

    function test_testnet_provided_healthy_then_oracle_only_eligible_real_unwind_profit() public {
        _openProvided();
        assertEq(pool.debtOf(market, borrower), 100e18);
        vm.prank(operator); vm.expectRevert(); executor.execute(_request(100e18, 1));
        oracle.setPriceFraction(4e17, 1e18);
        uint256 beforeAsset = asset.balanceOf(address(executor));
        uint256 gasBefore = gasleft();
        vm.prank(operator);
        (uint128 actualRepay, uint128 seized,, uint256 profit) = executor.execute(_request(100e18, 1e18));
        uint256 executionGas = gasBefore - gasleft();
        // Local bound only; the public runner uses the actual estimate and signed gas limit.
        (uint256 nativeCostInAsset,,,) = ITestnetQuoter(quoter).quoteExactOutput(abi.encodePacked(address(wrapped), FEE, address(asset)), (executionGas + executionGas / 2 + 21000) * 102 gwei);
        assertGe(profit, nativeCostInAsset);
        assertEq(actualRepay, 100e18); assertGt(seized, actualRepay); assertGt(profit, 100e18);
        assertEq(pool.debtOf(market, borrower), 0);
        assertEq(asset.balanceOf(address(executor)), beforeAsset + profit);
        assertEq(asset.allowance(address(executor), address(pool)), 0);
        assertEq(collateral.allowance(address(executor), router), 0);
        assertEq(asset.balanceOf(address(underlying)), receipt.totalSupply());
        executor.withdraw(address(asset), address(this), profit);
        assertEq(asset.balanceOf(address(executor)), beforeAsset);
    }

    function test_testnet_partial_then_full_and_profit_floor_atomic_rollback() public {
        _openProvided(); oracle.setPriceFraction(4e17, 1e18);
        uint128 oldDebt = pool.debtOf(market, borrower);
        uint128 oldCollateral = pool.collateralOf(market, borrower);
        vm.prank(operator); vm.expectRevert(); executor.execute(_request(50e18, 1_000_000e18));
        assertEq(pool.debtOf(market, borrower), oldDebt); assertEq(pool.collateralOf(market, borrower), oldCollateral);
        vm.prank(operator); executor.execute(_request(50e18, 1));
        assertEq(pool.debtOf(market, borrower), 50e18);
        // A partial liquidation may restore health; the oracle makes the remainder eligible again.
        oracle.setPriceFraction(2e17, 1e18);
        vm.prank(operator); executor.execute(_request(50e18, 1));
        assertEq(pool.debtOf(market, borrower), 0);
    }

    function test_testnet_maturity_after_real_grace_semantics_and_stale_equality() public {
        _openProvided();
        vm.warp(market + 61);
        oracle.setPriceFraction(1e18, 1e18);
        oracle.setObservation(1e18, 1e18, block.timestamp - 3600);
        vm.prank(operator); vm.expectRevert(abi.encodeWithSelector(ICreditPoolErrors.StaleOraclePrice.selector, block.timestamp - 3600));
        executor.execute(_request(100e18, 1));
        assertEq(pool.debtOf(market, borrower), 100e18);
        oracle.touch();
        vm.prank(operator); executor.execute(_request(100e18, 1));
        assertEq(pool.debtOf(market, borrower), 0);
    }

    function _openProvided() private {
        collateral.mint(borrower, 400e18);
        vm.startPrank(borrower);
        collateral.approve(address(pool), 400e18); pool.depositCollateral(borrower, market, 400e18);
        pool.placeBorrowOrder(borrower, market, 0, 100e18); vm.stopPrank();
        asset.mint(provider, 100e18);
        vm.startPrank(provider); asset.approve(address(pool), 100e18); pool.deposit(provider, 100e18);
        pool.provideExactIn(provider, market, 100e18, 0, 10); vm.stopPrank();
        assertEq(pool.debtOf(market, borrower), 100e18);
    }
    function _request(uint128 amount, uint256 minProfit) private view returns (LiquidationExecutor.Request memory) {
        return LiquidationExecutor.Request(address(pool), market, borrower, amount, 0, block.timestamp + 60,
            abi.encodePacked(address(collateral), FEE, address(asset)), minProfit, uint256(amount) + minProfit, keccak256(abi.encode(pool, borrower, amount, minProfit)));
    }
    function _artifact(string memory name, bytes memory args) private returns (address) {
        return _deploy(vm.parseJsonBytes(vm.readFile(string.concat("tests/fixtures/uniswap-v3/", name, ".json")), ".bytecode"), args);
    }
    function _deploy(bytes memory code, bytes memory args) private returns (address deployed) {
        bytes memory creation = bytes.concat(code, args);
        assembly ("memory-safe") { deployed := create(0, add(creation, 32), mload(creation)) }
        require(deployed != address(0) && deployed.code.length != 0, "deployment");
    }
    receive() external payable {}
}
