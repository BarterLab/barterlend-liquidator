// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ExecutorFixture, IFixtureFactory, IFixtureV3Pool } from "./ExecutorFixture.sol";
import { LiquidationExecutor, IExecutorRouter02 } from "../../contracts/LiquidationExecutor.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import { ERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";

contract CallbackProxy {
    function forward(LiquidationExecutor target,uint128 repay,uint128 seize,bytes calldata data) external {
        target.liquidateCallback(repay,seize,data);
    }
}

contract AdversarialCreditPool {
    address public asset;
    address public collateral;
    uint256 public constant version=5;
    uint8 public mode;
    uint128 public repay=100e18;
    uint128 public seize=120e18;
    CallbackProxy internal proxy=new CallbackProxy();
    constructor(address a,address c) {asset=a;collateral=c;}
    function configure(uint8 m,uint128 r,uint128 q) external {mode=m;repay=r;seize=q;}
    function liquidate(uint64 market,address borrower,uint128 requested,uint256 discount,uint256 deadline,bytes calldata data) external {
        if(mode==1)return;
        IERC20(collateral).transfer(msg.sender,seize);
        LiquidationExecutor target=LiquidationExecutor(msg.sender);
        if(mode==2){proxy.forward(target,repay,seize,data);return;}
        if(mode==3){target.liquidateCallback(repay,seize,bytes.concat(data,hex"00"));return;}
        if(mode==4){target.liquidateCallback(repay,seize+1,data);return;}
        if(mode==7){
            LiquidationExecutor.Request memory req=LiquidationExecutor.Request(address(this),market,borrower,requested,discount,deadline,"",1,1,keccak256("reentrant"));
            (bool ok,bytes memory result)=address(target).call(abi.encodeCall(target.execute,(req)));
            require(!ok && bytes4(result)==LiquidationExecutor.Busy.selector);
        }
        target.liquidateCallback(repay,seize,data);
        if(mode==5)target.liquidateCallback(repay,seize,data);
        if(mode!=6)IERC20(asset).transferFrom(msg.sender,address(this),repay);
    }
}

contract AdversarialRouter {
    address public factory;
    address public WETH9;
    IERC20 public asset;
    IERC20 public collateral;
    uint8 public mode;
    LiquidationExecutor public target;
    constructor(address f,address w,IERC20 a,IERC20 c){factory=f;WETH9=w;asset=a;collateral=c;}
    function configure(uint8 m,LiquidationExecutor t) external {mode=m;target=t;}
    function exactInput(IExecutorRouter02.ExactInputParams calldata args) external payable returns(uint256) {
        if(mode==1){return type(uint256).max;}
        if(mode==2){collateral.transferFrom(msg.sender,address(this),args.amountIn+1);return 0;}
        if(mode==3){
            (bool callbackOk,bytes memory callbackResult)=address(target).call(abi.encodeCall(target.liquidateCallback,(uint128(100e18),uint128(120e18),abi.encode(bytes32(0)))));
            require(!callbackOk && bytes4(callbackResult)==LiquidationExecutor.UnexpectedCallback.selector);
            LiquidationExecutor.Request memory req=LiquidationExecutor.Request(address(1),0,address(2),1,0,block.timestamp,"",1,1,keccak256("router reentrant"));
            (bool executeOk,bytes memory executeResult)=address(target).call(abi.encodeCall(target.execute,(req)));
            require(!executeOk && bytes4(executeResult)==LiquidationExecutor.Busy.selector);
            (bool withdrawOk,bytes memory withdrawResult)=address(target).call(abi.encodeCall(target.withdraw,(address(asset),address(this),1)));
            require(!withdrawOk && bytes4(withdrawResult)==LiquidationExecutor.Busy.selector);
        }
        uint256 spend=mode==4?args.amountIn/2:args.amountIn;
        collateral.transferFrom(msg.sender,address(this),spend);
        asset.transfer(args.recipient,120e18);
        return 120e18;
    }
}

contract FeeToken is ERC20 {
    constructor() ERC20("Fee token unsupported","FEE") {}
    function mint(uint256 amount) external {_mint(msg.sender,amount);}
    function _update(address from,address to,uint256 amount) internal override {
        if(from!=address(0)&&to!=address(0)){
            uint256 fee=amount/100;
            super._update(from,to,amount-fee);super._update(from,address(0),fee);
        }else super._update(from,to,amount);
    }
}

contract ReentrantTransferToken is ERC20 {
    LiquidationExecutor public target;
    address public creditPool;
    uint256 public phasesObserved;

    constructor() ERC20("Adversarial transfer hook","HOOK") {}
    function mint(uint256 amount) external {_mint(msg.sender,amount);}
    function configure(LiquidationExecutor executor,address pool) external {target=executor;creditPool=pool;}

    function _update(address from,address to,uint256 amount) internal override {
        super._update(from,to,amount);
        if(from!=address(target) || address(target)==address(0)) return;
        LiquidationExecutor.Phase current=target.phase();
        require(current==LiquidationExecutor.Phase.Repayment || current==LiquidationExecutor.Phase.Settling,"unexpected hook phase");
        phasesObserved |= 1<<uint256(current);
        LiquidationExecutor.Request memory request=LiquidationExecutor.Request(creditPool,0,address(1),1,0,block.timestamp,"",1,1,keccak256("transfer hook"));
        _assertBusy(abi.encodeCall(target.execute,(request)));
        _assertBusy(abi.encodeCall(target.withdraw,(address(this),address(this),1)));
        _assertBusy(abi.encodeCall(target.setVerifiedToken,(address(this),false)));
        _assertBusy(abi.encodeCall(target.transferOwnership,(address(1))));
        _assertBusy(abi.encodeCall(target.acceptOwnership,()));
        (bool ok,bytes memory result)=address(target).call(abi.encodeCall(target.liquidateCallback,(uint128(1),uint128(1),abi.encode(bytes32(0)))));
        require(!ok && bytes4(result)==LiquidationExecutor.UnexpectedCallback.selector,"callback reentry succeeded");
    }

    function _assertBusy(bytes memory payload) private {
        (bool ok,bytes memory result)=address(target).call(payload);
        require(!ok && bytes4(result)==LiquidationExecutor.Busy.selector,"transfer hook reentry succeeded");
    }
}

contract ExecutorSecurity is ExecutorFixture {
    AdversarialCreditPool internal fake;

    function beforeEach() public override {
        super.beforeEach();
        fake=new AdversarialCreditPool(address(asset),address(asset));
        vm.prank(address(fake));asset.mint(10_000e18);
        vm.prank(fixtureOwner);lendingFactory.registerPool(address(fake));
    }

    function _fakeRequest() internal view returns(LiquidationExecutor.Request memory) {
        return _request(address(fake),borrower_A,100e18,"",20e18);
    }

    function test_same_token_profit_equality_and_exact_allowance_clearing() public {
        vm.prank(address(executor));asset.mint(77e18);
        (uint128 r,uint128 q,uint256 spent,uint256 profit)=_execute(_fakeRequest());
        assertEq(r,100e18);assertEq(q,120e18);assertEq(spent,0);assertEq(profit,20e18);
        assertEq(asset.balanceOf(address(executor)),97e18);assertEq(asset.allowance(address(executor),address(fake)),0);
    }
    function test_callback_outside_execute_and_wrong_sender_are_rejected() public {
        vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);
        vm.prank(address(fake));executor.liquidateCallback(1,1,abi.encode(bytes32(0)));
        fake.configure(2,100e18,120e18);vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);_execute(_fakeRequest());
    }
    function test_no_callback_is_rejected() public {fake.configure(1,100e18,120e18);vm.expectRevert(LiquidationExecutor.MissingCallback.selector);_execute(_fakeRequest());}
    function test_repeated_callback_is_rejected() public {fake.configure(5,100e18,120e18);vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);_execute(_fakeRequest());}
    function test_wrong_callback_data_is_rejected() public {fake.configure(3,100e18,120e18);vm.expectRevert(LiquidationExecutor.UnexpectedCallback.selector);_execute(_fakeRequest());}
    function test_actual_received_delta_mismatch_is_rejected() public {fake.configure(4,100e18,120e18);vm.expectRevert(LiquidationExecutor.CallbackAmountsMismatch.selector);_execute(_fakeRequest());}
    function test_zero_input_and_actual_repay_above_request_are_rejected() public {
        fake.configure(0,100e18,0);vm.expectRevert(LiquidationExecutor.CallbackAmountsMismatch.selector);_execute(_fakeRequest());
        fake.configure(0,101e18,120e18);vm.expectRevert(LiquidationExecutor.CallbackAmountsMismatch.selector);_execute(_fakeRequest());
    }
    function test_pool_not_pulling_repayment_is_rejected() public {fake.configure(6,100e18,120e18);vm.expectRevert(LiquidationExecutor.RepaymentMismatch.selector);_execute(_fakeRequest());}
    function test_authorized_pool_reentry_is_blocked_in_waiting_phase() public {
        vm.prank(fixtureOwner);executor.setOperator(address(fake),true);fake.configure(7,100e18,120e18);_execute(_fakeRequest());
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }
    function test_old_assets_cannot_subsidize_unprofitable_callback() public {
        vm.prank(address(executor));asset.mint(10_000e18);fake.configure(0,100e18,100e18);
        vm.expectRevert(LiquidationExecutor.InsufficientProfit.selector);_execute(_fakeRequest());assertEq(asset.balanceOf(address(executor)),10_000e18);
    }
    function test_only_operator_owner_and_valid_request() public {
        LiquidationExecutor.Request memory req=_fakeRequest();vm.expectRevert(LiquidationExecutor.Unauthorized.selector);executor.execute(req);
        req.minAssetProfit=0;vm.expectRevert(LiquidationExecutor.InvalidRequest.selector);_execute(req);
        req=_fakeRequest();req.planFingerprint=bytes32(0);vm.expectRevert(LiquidationExecutor.InvalidRequest.selector);_execute(req);
        vm.expectRevert(LiquidationExecutor.Unauthorized.selector);executor.withdraw(address(asset),address(this),1);
    }
    function test_native_and_unverified_fee_token_configuration_rejected() public {
        vm.expectRevert(LiquidationExecutor.InvalidAddress.selector);vm.prank(fixtureOwner);executor.setVerifiedToken(address(0),true);
        FeeToken token=new FeeToken();AdversarialCreditPool feePool=new AdversarialCreditPool(address(token),address(token));
        vm.prank(fixtureOwner);lendingFactory.registerPool(address(feePool));
        vm.expectRevert(LiquidationExecutor.UnsupportedToken.selector);_execute(_request(address(feePool),borrower_A,100e18,"",1));
    }
    function test_forced_trust_of_fee_collateral_fails_actual_delta_guard() public {
        FeeToken token=new FeeToken();AdversarialCreditPool feePool=new AdversarialCreditPool(address(token),address(token));
        vm.prank(address(feePool));token.mint(10_000e18);
        vm.prank(fixtureOwner);executor.setVerifiedToken(address(token),true);
        vm.prank(fixtureOwner);lendingFactory.registerPool(address(feePool));
        vm.expectRevert(LiquidationExecutor.CallbackAmountsMismatch.selector);_execute(_request(address(feePool),borrower_A,100e18,"",1));
    }
    function test_fee_on_repayment_asset_is_rejected_by_pool_received_guard() public {
        FeeToken token=new FeeToken();
        address pair=IFixtureFactory(factory).createPool(address(token),address(collateral),3000);
        IFixtureV3Pool(pair).initialize(Q96);
        AdversarialRouter swapper=new AdversarialRouter(factory,address(wrapped),IERC20(address(token)),collateral);
        LiquidationExecutor target=new LiquidationExecutor(fixtureOwner,OPERATOR,address(swapper),factory,_lendingFactories());
        AdversarialCreditPool credit=new AdversarialCreditPool(address(token),address(collateral));
        vm.prank(address(credit));collateral.mint(10_000e18);vm.prank(address(swapper));token.mint(10_000e18);
        vm.prank(fixtureOwner);target.setVerifiedToken(address(token),true);vm.prank(fixtureOwner);target.setVerifiedToken(address(collateral),true);
        vm.prank(fixtureOwner);target.setFee(3000,true);vm.prank(fixtureOwner);lendingFactory.registerPool(address(credit));
        bytes memory path=abi.encodePacked(address(collateral),uint24(3000),address(token));
        vm.expectRevert(LiquidationExecutor.RepaymentMismatch.selector);vm.prank(OPERATOR);target.execute(_request(address(credit),borrower_A,100e18,path,1));
    }
    function test_withdraw_and_two_step_ownership_only_when_idle() public {
        vm.prank(address(executor)); asset.mint(77e18);
        vm.prank(address(executor)); collateral.mint(33e18);
        wrapped.transfer(address(executor),11e18);
        _execute(_fakeRequest());
        vm.expectRevert(LiquidationExecutor.Unauthorized.selector);
        vm.prank(OPERATOR); executor.withdraw(address(asset),provider_B,20e18);
        vm.prank(fixtureOwner); executor.withdraw(address(asset),provider_B,20e18);
        assertEq(asset.balanceOf(provider_B),20e18);
        assertEq(asset.balanceOf(address(executor)),77e18);
        assertEq(collateral.balanceOf(address(executor)),33e18);
        assertEq(wrapped.balanceOf(address(executor)),11e18);
        assertEq(asset.allowance(address(executor),address(fake)),0);
        assertEq(collateral.allowance(address(executor),router),0);
        assertEq(uint256(executor.phase()),uint256(LiquidationExecutor.Phase.Idle));
        vm.prank(fixtureOwner);executor.transferOwnership(provider_B);assertEq(executor.owner(),fixtureOwner);
        vm.expectRevert(LiquidationExecutor.Unauthorized.selector);executor.acceptOwnership();
        vm.prank(provider_B);executor.acceptOwnership();assertEq(executor.owner(),provider_B);
    }

    function test_revoked_operator_cannot_execute_until_explicitly_reenabled() public {
        vm.prank(fixtureOwner); executor.setOperator(OPERATOR,false);
        vm.expectRevert(LiquidationExecutor.Unauthorized.selector);
        _execute(_fakeRequest());
        assertEq(asset.balanceOf(address(executor)),0);
        vm.prank(fixtureOwner); executor.setOperator(OPERATOR,true);
        (,,,uint256 profit)=_execute(_fakeRequest());
        assertEq(profit,20e18);
    }

    function test_revoked_token_blocks_previously_enabled_pool_until_reverified() public {
        vm.prank(fixtureOwner); executor.setVerifiedToken(address(asset),false);
        vm.expectRevert(LiquidationExecutor.UnsupportedToken.selector);
        _execute(_fakeRequest());
        assertEq(asset.balanceOf(address(executor)),0);
        vm.prank(fixtureOwner); executor.setVerifiedToken(address(asset),true);
        (,,,uint256 profit)=_execute(_fakeRequest());
        assertEq(profit,20e18);
    }

    function test_unrecognized_pool_cannot_callback_or_spend_existing_assets() public {
        vm.prank(address(executor)); asset.mint(77e18);
        vm.mockCall(address(lendingFactory),abi.encodeWithSignature("isPool(address)",address(fake)),abi.encode(false));
        vm.expectRevert(LiquidationExecutor.UnsupportedPool.selector);
        _execute(_fakeRequest());
        assertEq(asset.balanceOf(address(executor)),77e18);
        assertEq(asset.allowance(address(executor),address(fake)),0);
    }

    function test_authorized_token_owner_reentry_during_repayment_and_withdraw_settling_is_blocked() public {
        ReentrantTransferToken token=new ReentrantTransferToken();
        AdversarialCreditPool credit=new AdversarialCreditPool(address(token),address(token));
        LiquidationExecutor target=new LiquidationExecutor(fixtureOwner,OPERATOR,router,factory,_lendingFactories());
        token.configure(target,address(credit));
        vm.prank(address(credit)); token.mint(10_000e18);
        vm.prank(address(target)); token.mint(77e18);
        vm.prank(fixtureOwner); target.setVerifiedToken(address(token),true);
        vm.prank(fixtureOwner); lendingFactory.registerPool(address(credit));
        vm.prank(fixtureOwner); target.setOperator(address(token),true);
        vm.prank(fixtureOwner); target.transferOwnership(address(token));
        vm.prank(address(token)); target.acceptOwnership();
        vm.prank(OPERATOR); (uint128 repaid,uint128 seized,uint256 spent,uint256 profit)=target.execute(_request(address(credit),borrower_A,100e18,"",20e18));
        assertEq(repaid,100e18);assertEq(seized,120e18);assertEq(spent,0);assertEq(profit,20e18);
        assertEq(token.phasesObserved(),8);
        assertEq(token.balanceOf(address(target)),97e18);
        assertEq(token.allowance(address(target),address(credit)),0);
        vm.prank(address(token)); target.withdraw(address(token),provider_B,20e18);
        assertEq(token.phasesObserved(),24);
        assertEq(token.balanceOf(provider_B),20e18);
        assertEq(token.balanceOf(address(target)),77e18);
        assertEq(token.allowance(address(target),address(credit)),0);
        assertEq(target.owner(),address(token));
        assertEq(target.pendingOwner(),address(0));
        assertTrue(target.verifiedTokens(address(token)));
        (,,bool enabled)=target.poolConfig(address(credit));
        assertTrue(enabled);
        assertEq(uint256(target.phase()),uint256(LiquidationExecutor.Phase.Idle));
    }

    function _mockRouter(uint8 mode) internal returns(LiquidationExecutor target,AdversarialRouter swapper,AdversarialCreditPool credit) {
        swapper=new AdversarialRouter(factory,address(wrapped),asset,collateral);
        target=new LiquidationExecutor(fixtureOwner,OPERATOR,address(swapper),factory,_lendingFactories());
        credit=new AdversarialCreditPool(address(asset),address(collateral));
        vm.prank(address(credit));collateral.mint(10_000e18);vm.prank(address(swapper));asset.mint(10_000e18);
        vm.prank(fixtureOwner);target.setVerifiedToken(address(asset),true);vm.prank(fixtureOwner);target.setVerifiedToken(address(collateral),true);
        vm.prank(fixtureOwner);target.setFee(3000,true);vm.prank(fixtureOwner);lendingFactory.registerPool(address(credit));
        swapper.configure(mode,target);
    }
    function test_falsified_router_return_does_not_replace_asset_balance_guard() public {
        (LiquidationExecutor target,,AdversarialCreditPool credit)=_mockRouter(1);
        vm.expectRevert(LiquidationExecutor.InsufficientProfit.selector);vm.prank(OPERATOR);target.execute(_request(address(credit),borrower_A,100e18,_path(3000),1));
    }
    function test_router_cannot_overspend_old_collateral_via_allowance() public {
        (LiquidationExecutor target,,AdversarialCreditPool credit)=_mockRouter(2);vm.prank(address(target));collateral.mint(77e18);
        vm.expectRevert();vm.prank(OPERATOR);target.execute(_request(address(credit),borrower_A,100e18,_path(3000),1));assertEq(collateral.balanceOf(address(target)),77e18);
    }
    function test_owner_router_reentrancy_and_spoofed_callback_are_blocked() public {
        (LiquidationExecutor target,AdversarialRouter swapper,AdversarialCreditPool credit)=_mockRouter(3);
        vm.prank(fixtureOwner);target.setOperator(address(swapper),true);vm.prank(fixtureOwner);target.transferOwnership(address(swapper));vm.prank(address(swapper));target.acceptOwnership();
        vm.prank(OPERATOR);target.execute(_request(address(credit),borrower_A,100e18,_path(3000),20e18));
        assertEq(asset.balanceOf(address(target)),20e18);assertEq(collateral.allowance(address(target),address(swapper)),0);
    }
    function test_partial_swap_profit_only_asset_residual_not_counted() public {
        (LiquidationExecutor target,AdversarialRouter swapper,AdversarialCreditPool credit)=_mockRouter(4);vm.prank(address(target));collateral.mint(77e18);
        vm.prank(OPERATOR);(,,uint256 spent,uint256 profit)=target.execute(_request(address(credit),borrower_A,100e18,_path(3000),20e18));
        assertEq(spent,60e18);assertEq(profit,20e18);assertEq(collateral.balanceOf(address(target)),137e18);assertEq(collateral.allowance(address(target),address(swapper)),0);
    }
}
