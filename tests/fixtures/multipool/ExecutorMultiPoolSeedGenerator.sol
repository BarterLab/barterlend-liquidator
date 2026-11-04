// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ExecutorFixture } from "tests/solidity/ExecutorFixture.sol";
import { MockCreditPool } from "test/CreditPool/0_base.t.sol";
import { MockPriceOracle } from "test/mocks/MockPriceOracle.sol";

// Copied into a temporary Foundry test directory by test-local-multipool.sh.
contract ExecutorMultiPoolSeedGenerator is ExecutorFixture {
    MockCreditPool internal otherPool;
    MockPriceOracle internal otherOracle;
    MockCreditPool[] internal bulkPools;
    MockPriceOracle[] internal bulkOracles;
    uint256 internal perMarket;
    uint256 internal eligiblePerMarket;

    function beforeEach() public override {
        super.beforeEach();
        (otherPool,otherOracle)=_newPool(asset,collateral);
        uint256 count=vm.envOr("BARTERLEND_BULK_POOLS",uint256(0));
        if(count!=0) {
            require(count==4 || count==8,"bounded bulk profile");
            perMarket=count==4?150:250;eligiblePerMarket=count==4?4:2;
            bulkPools.push(pool);bulkOracles.push(priceOracle);
            bulkPools.push(otherPool);bulkOracles.push(otherOracle);
            for(uint256 i=2;i<count;++i) {
                (MockCreditPool next,MockPriceOracle oracle)=_newPool(asset,collateral);
                bulkPools.push(next);bulkOracles.push(oracle);
            }
            for(uint256 i;i<count;++i) {
                MockCreditPool credit=bulkPools[i];
                vm.startPrank(provider_A);
                asset.mint(perMarket*2*100e18);asset.approve(address(credit),type(uint256).max);
                credit.deposit(provider_A,uint128(perMarket*2*100e18));
                for(uint256 m;m<2;++m) credit.placeProvideOrder(provider_A,markets[m],0,uint128(perMarket*100e18));
                vm.stopPrank();
                for(uint256 m;m<2;++m) for(uint256 j;j<perMarket;++j) {
                    address borrower=address(uint160(0x100000+i*1000+m*500+j));
                    uint128 pledge=j<eligiblePerMarket?400e18:800e18;
                    vm.startPrank(borrower);
                    collateral.mint(pledge);collateral.approve(address(credit),pledge);
                    credit.depositCollateral(borrower,markets[m],pledge);
                    credit.borrow(borrower,markets[m],100e18,100e18);
                    vm.stopPrank();assertEq(credit.debtOf(markets[m],borrower),100e18);
                }
                bulkOracles[i].setPrice(0.4e18);
            }
        }
        _exportSeed();
    }

    function test_generate_isolated_two_pool_seed() public {
        assertTrue(address(pool)!=address(otherPool));
        assertTrue(address(priceOracle)!=address(otherOracle));
        assertEq(pool.version(),5);assertEq(otherPool.version(),5);
        assertEq(pool.debtOf(markets[0],borrower_A),0);assertEq(otherPool.debtOf(markets[0],borrower_A),0);
        assertEq(pool.collateralOf(markets[0],borrower_A),0);assertEq(otherPool.collateralOf(markets[0],borrower_A),0);
        (,,bool firstEnabled)=executor.poolConfig(address(pool));
        (,,bool secondEnabled)=executor.poolConfig(address(otherPool));
        assertTrue(firstEnabled && secondEnabled);
    }

    // Export in setup, while Foundry still retains every constructor account in its journal.
    function _exportSeed() private {
        string memory output=vm.envString("BARTERLEND_MULTIPOOL_OUTPUT_DIR");
        vm.dumpState(string.concat(output,"/forge-state.json"));
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
        manifest=string.concat(manifest,',"runtime_code_hashes":{"factory":"',vm.toString(factory.codehash),'","router":"',vm.toString(router.codehash),'","quoter":"',vm.toString(quoter.codehash),'"');
        manifest=string.concat(manifest,',"wrapped_native":"',vm.toString(address(wrapped).codehash),'","executor":"',vm.toString(address(executor).codehash),'","pool":"',vm.toString(address(pool).codehash),'"}');
        manifest=string.concat(manifest,',"pool_b":"',vm.toString(address(otherPool)),'","oracle_b":"',vm.toString(address(otherOracle)),'","pool_b_code_hash":"',vm.toString(address(otherPool).codehash),'","oracle_b_code_hash":"',vm.toString(address(otherOracle).codehash),'"}');
        if(bulkPools.length!=0) {
            bytes memory base=bytes(manifest);assembly("memory-safe"){mstore(base,sub(mload(base),1))}
            manifest=string.concat(string(base),',"positions_per_market":',vm.toString(perMarket),',"eligible_per_market":',vm.toString(eligiblePerMarket),',"credit_pools":[');
            for(uint256 i;i<bulkPools.length;++i) {
                if(i!=0) manifest=string.concat(manifest,',');
                manifest=string.concat(manifest,'{"address":"',vm.toString(address(bulkPools[i])),'","oracle":"',vm.toString(address(bulkOracles[i])),'","code_hash":"',vm.toString(address(bulkPools[i]).codehash),'","oracle_code_hash":"',vm.toString(address(bulkOracles[i]).codehash),'"}');
            }
            manifest=string.concat(manifest,']}');
        }
        vm.writeFile(string.concat(output,"/manifest.json"),manifest);
    }

    function _poolJson(address amm,address a,address b,uint24 fee) private view returns(string memory) {
        return string.concat('{"address":"',vm.toString(amm),'","token_a":"',vm.toString(a),'","token_b":"',vm.toString(b),'","fee":',vm.toString(fee),'}');
    }
}
