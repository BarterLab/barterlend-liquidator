// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { CreditPoolTestSuite, MockCreditPool } from "test/CreditPool/0_base.t.sol";
import { MockToken } from "test/mocks/MockToken.sol";
import { MockPriceOracle } from "test/mocks/MockPriceOracle.sol";
import { MockAaveToken } from "test/mocks/MockAaveToken.sol";
import { MockAavePool } from "test/mocks/MockAavePool.sol";
import { CreditPoolFacet1 } from "src/facets/CreditPoolFacet1.sol";
import { CreditPoolFacet2 } from "src/facets/CreditPoolFacet2.sol";
import { CreditPoolFacet3 } from "src/facets/CreditPoolFacet3.sol";
import { CreditPoolFacet4 } from "src/facets/CreditPoolFacet4.sol";
import { UnderlyingPoolConfig } from "src/interfaces/IUnderlyingPool.sol";
import { MarketsConfigParams } from "src/lib/MarketsConfig.sol";
import { TestLendingFactory } from "../fixtures/factory/TestLendingFactory.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import { ERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";

interface IFixtureFactory {
    function createPool(address a, address b, uint24 fee) external returns (address);
}
interface IFixtureV3Pool {
    function initialize(uint160 price) external;
    function mint(address who, int24 lower, int24 upper, uint128 liquidity, bytes calldata data) external returns (uint256, uint256);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function liquidity() external view returns (uint128);
}
interface IFixtureQuoter {
    function quoteExactInput(bytes calldata path, uint256 amount) external returns (uint256 out, uint160[] memory prices, uint32[] memory ticks, uint256 gasEstimate);
}

contract WrappedLocalMON is ERC20 {
    constructor() ERC20("Local Wrapped MON", "WMON") {}
    function deposit() external payable { _mint(msg.sender, msg.value); }
    function withdraw(uint256 amount) external { _burn(msg.sender, amount); (bool ok,) = msg.sender.call{value: amount}(""); require(ok); }
}

abstract contract ExecutorFixture is CreditPoolTestSuite {
    address internal fixtureOwner = makeAddr("Executor fixture owner");
    address internal constant OPERATOR = 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266;
    uint160 internal constant Q96 = 79228162514264337593543950336;
    address internal factory;
    address internal router;
    address internal quoter;
    WrappedLocalMON internal wrapped;
    LiquidationExecutor internal executor;
    TestLendingFactory internal lendingFactory;
    address internal expectedMintPool;
    address internal directPool;
    address internal lowFeePool;
    address internal firstHop;
    address internal secondHop;
    address internal thinDirect;
    address internal thinFirst;
    address internal thinSecond;

    function _owner() internal view override returns (address) { return fixtureOwner; }

    function beforeEach() public virtual override {
        vm.deal(address(this), 1_000_000_000e18);
        vm.deal(OPERATOR, 100e18);
        vm.deal(fixtureOwner, 100e18);
        vm.deal(provider_A, 100e18);
        vm.deal(provider_B, 100e18);
        vm.deal(borrower_A, 100e18);
        vm.deal(borrower_B, 100e18);
        wrapped = new WrappedLocalMON();
        wrapped.deposit{value: 100_000_000e18}();
        asset.mint(100_000_000e18);
        collateral.mint(100_000_000e18);
        factory = _deployArtifact("UniswapV3Factory", "");
        require(keccak256(_creation("UniswapV3Pool")) == 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54);
        router = _deployArtifact("SwapRouter02", abi.encode(address(0), factory, address(0), address(wrapped)));
        quoter = _deployArtifact("QuoterV2", abi.encode(factory, address(wrapped)));
        directPool = _liquidPool(address(collateral), address(asset), 3000, -887220, 887220, 1_000_000e18);
        lowFeePool = _liquidPool(address(collateral), address(asset), 500, -887270, 887270, 1_000_000e18);
        firstHop = _liquidPool(address(collateral), address(wrapped), 3000, -887220, 887220, 1_000_000e18);
        secondHop = _liquidPool(address(wrapped), address(asset), 3000, -887220, 887220, 1_000_000e18);
        thinDirect = _liquidPool(address(collateral), address(asset), 10000, -6000, 6000, 200e18);
        thinFirst = _liquidPool(address(collateral), address(wrapped), 10000, -6000, 6000, 200e18);
        thinSecond = _liquidPool(address(wrapped), address(asset), 10000, -6000, 6000, 200e18);
        lendingFactory = new TestLendingFactory(fixtureOwner);
        executor = new LiquidationExecutor(fixtureOwner, OPERATOR, router, factory, _lendingFactories());
        vm.prank(fixtureOwner); executor.setVerifiedToken(address(asset), true);
        vm.prank(fixtureOwner); executor.setVerifiedToken(address(collateral), true);
        vm.prank(fixtureOwner); executor.setVerifiedToken(address(wrapped), true);
        vm.prank(fixtureOwner); executor.setIntermediate(address(wrapped), true);
        vm.prank(fixtureOwner); executor.setFee(500, true);
        vm.prank(fixtureOwner); executor.setFee(3000, true);
        vm.prank(fixtureOwner); executor.setFee(10000, true);
        vm.prank(fixtureOwner); lendingFactory.registerPool(address(pool));
    }

    function _lendingFactories() internal view returns(address[] memory values) {
        values=new address[](1);values[0]=address(lendingFactory);
    }

    function _openProvided(address borrower) internal {
        _deposit_collateral(borrower, markets[0], 400e18);
        vm.prank(borrower); pool.placeBorrowOrder(borrower, markets[0], 1000, 100e18);
        assertEq(pool.debtOf(markets[0], borrower), 0);
        _deposit(provider_A, 100e18);
        vm.prank(provider_A); pool.provideExactIn(provider_A, markets[0], 100e18, 0, 10);
        assertEq(pool.debtOf(markets[0], borrower), 110e18);
    }

    function _request(address creditPool, address borrower, uint128 amount, bytes memory path, uint256 minProfit) internal view returns (LiquidationExecutor.Request memory) {
        return LiquidationExecutor.Request({pool: creditPool, market: markets[0], borrower: borrower,
            requestedDebt: amount, minDiscountOut: 0, deadline: block.timestamp + 60,
            path: path, minAssetProfit: minProfit, minSwapOutput: 1, planFingerprint: keccak256(abi.encode(creditPool,borrower,amount,path,minProfit))});
    }

    function _path(uint24 fee) internal view returns (bytes memory) { return abi.encodePacked(address(collateral), fee, address(asset)); }
    function _twoHop(uint24 feeA, uint24 feeB) internal view returns (bytes memory) { return abi.encodePacked(address(collateral),feeA,address(wrapped),feeB,address(asset)); }

    function _execute(LiquidationExecutor.Request memory request) internal returns (uint128 repaid, uint128 seized, uint256 spent, uint256 profit) {
        vm.prank(OPERATOR); return executor.execute(request);
    }

    function _liquidPool(address a, address b, uint24 fee, int24 lower, int24 upper, uint128 liquidity) internal returns (address amm) {
        amm = IFixtureFactory(factory).createPool(a,b,fee);
        IFixtureV3Pool(amm).initialize(Q96);
        expectedMintPool = amm;
        IFixtureV3Pool(amm).mint(address(this),lower,upper,liquidity,"");
        expectedMintPool = address(0);
    }

    function uniswapV3MintCallback(uint256 a, uint256 b, bytes calldata) external {
        require(msg.sender == expectedMintPool && expectedMintPool != address(0));
        if(a != 0) IERC20(IFixtureV3Pool(msg.sender).token0()).transfer(msg.sender,a);
        if(b != 0) IERC20(IFixtureV3Pool(msg.sender).token1()).transfer(msg.sender,b);
    }

    function _creation(string memory name) internal view returns (bytes memory) {
        return vm.parseJsonBytes(vm.readFile(string.concat("tests/fixtures/uniswap-v3/",name,".json")),".bytecode");
    }

    function _deployArtifact(string memory name, bytes memory args) internal returns (address deployed) {
        bytes memory creation = bytes.concat(_creation(name),args);
        assembly ("memory-safe") { deployed := create(0,add(creation,32),mload(creation)) }
        require(deployed != address(0) && deployed.code.length != 0);
    }

    function _newPool(MockToken debtToken, MockToken pledgeToken) internal returns (MockCreditPool fresh, MockPriceOracle oracle) {
        bytes memory initCode;
        (initCode,oracle)=_poolInitCode(debtToken,pledgeToken);
        vm.prank(fixtureOwner); fresh=MockCreditPool(lendingFactory.createPool(initCode));
    }

    function _poolInitCode(MockToken debtToken, MockToken pledgeToken) internal returns(bytes memory initCode, MockPriceOracle oracle) {
        MockAaveToken receipt = new MockAaveToken(debtToken,10);
        MockAavePool underlying = new MockAavePool(debtToken,receipt);
        underlying;
        oracle = new MockPriceOracle(address(pledgeToken),address(debtToken));
        address[] memory facets = new address[](4);
        facets[0]=address(new CreditPoolFacet1()); facets[1]=address(new CreditPoolFacet2());
        facets[2]=address(new CreditPoolFacet3()); facets[3]=address(new CreditPoolFacet4());
        InitializationParams memory params = InitializationParams({owner:fixtureOwner,permit2:address(permit2),
            asset:address(debtToken),underlyingPool:UnderlyingPoolConfig({adapter:address(aaveAdapter),config:bytes32(uint256(uint160(address(receipt))))}),
            collateral:address(pledgeToken),priceOracle:address(oracle),stalePriceWindow:1 days,
            protocolFee:0,totalCollateralCap:type(uint128).max,provideOrderMinAmount:1e18,borrowOrderMinAmount:1e18,
            maxActiveProvideOrders:8,maxActiveBorrowOrders:1,borrowLTV:UNIT/2,liquidationLTV:UNIT/2,
            liquidationDiscountMin:UNIT/20,liquidationDiscountMax:UNIT/5,minDebtRemainingAfterLiquidation:10e18,liquidationGracePeriod:1 days,
            marketsConfig:MarketsConfigParams({durations:[YEAR,YEAR,YEAR],active:[uint8(3),3,3],extensions:[uint64(0),0,0]})});
        initCode=bytes.concat(type(MockCreditPool).creationCode,abi.encode(params,facets));
    }

    function _fundPool(MockCreditPool creditPool, MockToken debtToken, MockToken pledgeToken, address borrower) internal {
        vm.prank(provider_B); debtToken.mint(100e18);
        vm.prank(provider_B); debtToken.approve(address(creditPool),100e18);
        vm.prank(provider_B); creditPool.deposit(provider_B,100e18);
        vm.prank(provider_B); creditPool.placeProvideOrder(provider_B,markets[0],0,100e18);
        vm.prank(borrower); pledgeToken.mint(400e18);
        vm.prank(borrower); pledgeToken.approve(address(creditPool),400e18);
        vm.prank(borrower); creditPool.depositCollateral(borrower,markets[0],400e18);
        vm.prank(borrower); creditPool.borrow(borrower,markets[0],100e18,100e18);
    }
}

contract ExecutorSeedGenerator is ExecutorFixture {
    function beforeEach() public override {
        super.beforeEach();
        (bytes memory nextPoolInitCode,)=_poolInitCode(asset,collateral);
        vm.writeFile("tests/fixtures/solidity/generated/new-pool-initcode.hex",vm.toString(nextPoolInitCode));
        vm.dumpState("tests/fixtures/solidity/generated/forge-state.json");
        string memory manifest = '{"chain_id":31337,"genesis_hash":"0x3e8b48d8cbf5dfa86c941f2c1821786417d34ef516ef1a336fdb035f984f89e9","deployment_block":0,"timestamp":1';
        manifest=string.concat(manifest,',"pool":"',vm.toString(address(pool)),'","asset":"',vm.toString(address(asset)),'","collateral":"',vm.toString(address(collateral)),'"');
        manifest=string.concat(manifest,',"oracle":"',vm.toString(address(priceOracle)),'","factory":"',vm.toString(factory),'","router":"',vm.toString(router),'"');
        manifest=string.concat(manifest,',"quoter":"',vm.toString(quoter),'","wrapped_native":"',vm.toString(address(wrapped)),'","middle":"',vm.toString(address(wrapped)),'"');
        manifest=string.concat(manifest,',"executor":"',vm.toString(address(executor)),'","owner":"',vm.toString(fixtureOwner),'","operator":"',vm.toString(OPERATOR),'"');
        manifest=string.concat(manifest,',"provider_a":"',vm.toString(provider_A),'","provider_b":"',vm.toString(provider_B),'","borrower_a":"',vm.toString(borrower_A),'"');
        manifest=string.concat(manifest,',"borrower_b":"',vm.toString(borrower_B),'","market0":"',vm.toString(markets[0]),'","market1":"',vm.toString(markets[1]),'","pools":[');
        manifest=string.concat(manifest,_poolJson(directPool,address(collateral),address(asset),3000),',',_poolJson(lowFeePool,address(collateral),address(asset),500),',',_poolJson(firstHop,address(collateral),address(wrapped),3000),',');
        manifest=string.concat(manifest,_poolJson(secondHop,address(wrapped),address(asset),3000),',',_poolJson(thinDirect,address(collateral),address(asset),10000),',');
        manifest=string.concat(manifest,_poolJson(thinFirst,address(collateral),address(wrapped),10000),',',_poolJson(thinSecond,address(wrapped),address(asset),10000),']');
        manifest=string.concat(manifest,',"lending_factory":"',vm.toString(address(lendingFactory)),'","lending_factory_code_hash":"',vm.toString(address(lendingFactory).codehash),'"');
        manifest=string.concat(manifest,',"runtime_code_hashes":{"factory":"',vm.toString(factory.codehash),'","router":"',vm.toString(router.codehash),'","quoter":"',vm.toString(quoter.codehash),'"');
        manifest=string.concat(manifest,',"wrapped_native":"',vm.toString(address(wrapped).codehash),'","executor":"',vm.toString(address(executor).codehash),'","pool":"',vm.toString(address(pool).codehash),'","oracle":"',vm.toString(address(priceOracle).codehash),'"}}');
        vm.writeFile("tests/fixtures/solidity/generated/manifest.json",manifest);
    }
    function test_generate_empty_executor_seed() public {
        assertEq(pool.debtOf(markets[0],borrower_A),0);
        assertEq(pool.collateralOf(markets[0],borrower_A),0);
        assertEq(asset.balanceOf(address(executor)),0);
        assertEq(executor.owner(),fixtureOwner);
        assertTrue(executor.operators(OPERATOR));
    }
    function _poolJson(address amm,address a,address b,uint24 fee) private view returns(string memory) {
        return string.concat('{"address":"',vm.toString(amm),'","token_a":"',vm.toString(a),'","token_b":"',vm.toString(b),'","fee":',vm.toString(fee),'}');
    }
}
