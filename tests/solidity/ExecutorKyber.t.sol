// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;
import { ExecutorFixture } from "./ExecutorFixture.sol";
import { AdversarialCreditPool } from "./ExecutorSecurity.t.sol";
import { LiquidationExecutor } from "../../contracts/LiquidationExecutor.sol";
import { IKyberRouter } from "../../contracts/KyberRoute.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

contract LocalKyberRouter is IKyberRouter {
    uint256 public mode;
    function setMode(uint256 value) external { mode = value; }
    function swap(Execution calldata e) external payable returns (uint256, uint256) {
        require(msg.value == 0);
        if (mode == 1) return (type(uint256).max, 0); // Lie about output.
        uint256 amount = e.desc.amount + (mode == 2 ? 1 : 0);
        IERC20(e.desc.srcToken).transferFrom(msg.sender, address(this), amount);
        if (mode == 3) {
            (bool ok,) = msg.sender.call(abi.encodeWithSelector(LiquidationExecutor.liquidateCallback.selector, uint128(100e18), uint128(120e18), hex"00"));
            require(!ok, "callback reentry succeeded");
        }
        IERC20(e.desc.dstToken).transfer(e.desc.dstReceiver == address(0) ? msg.sender : e.desc.dstReceiver, 120e18);
        return (120e18, 0);
    }
}

contract ExecutorKyber is ExecutorFixture {
    LocalKyberRouter internal kyber;
    AdversarialCreditPool internal credit;
    function beforeEach() public override {
        super.beforeEach();
        kyber = new LocalKyberRouter(); credit = new AdversarialCreditPool(address(asset),address(collateral));
        collateral.transfer(address(credit), 10000e18); asset.transfer(address(kyber),10000e18);
        vm.startPrank(fixtureOwner);
        lendingFactory.registerPool(address(credit));
        executor.setKyberRouter(address(kyber), address(kyber).codehash);
        vm.stopPrank();
    }
    function _execution() internal view returns (IKyberRouter.Execution memory e) {
        e.callTarget = address(kyber); e.approveTarget = address(kyber); e.targetData = hex"1234";
        e.desc.srcToken = address(collateral); e.desc.dstToken = address(asset); e.desc.dstReceiver = address(executor);
        e.desc.srcReceivers = new address[](1); e.desc.srcReceivers[0] = address(kyber);
        e.desc.srcAmounts = new uint256[](1); e.desc.srcAmounts[0] = 120e18;
        e.desc.amount = 120e18; e.desc.minReturnAmount = 120e18;
    }
    function _kyberRequest(IKyberRouter.Execution memory e) internal view returns (LiquidationExecutor.Request memory r) {
        r = _request(address(credit),borrower_A,100e18,abi.encode(bytes4(0x4b594252),address(kyber),abi.encodeCall(IKyberRouter.swap,(e))),20e18);
        r.minSwapOutput = 120e18;
    }
    function test_kyber_profit_old_balances_and_zero_allowances() public {
        asset.transfer(address(executor),77e18); collateral.transfer(address(executor),33e18);
        (uint128 repaid,uint128 seized,uint256 spent,uint256 profit) = _execute(_kyberRequest(_execution()));
        assertEq(repaid,100e18); assertEq(seized,120e18); assertEq(spent,120e18); assertEq(profit,20e18);
        assertEq(asset.balanceOf(address(executor)),97e18); assertEq(collateral.balanceOf(address(executor)),33e18);
        assertEq(collateral.allowance(address(executor),address(kyber)),0); assertEq(asset.allowance(address(executor),address(credit)),0);
    }
    function test_kyber_bad_output_and_old_balance_overpull_revert_atomically() public {
        asset.transfer(address(executor),77e18); collateral.transfer(address(executor),33e18);
        for (uint256 mode=1; mode<=2; ++mode) {
            kyber.setMode(mode); vm.expectRevert(); _execute(_kyberRequest(_execution()));
            assertEq(asset.balanceOf(address(executor)),77e18); assertEq(collateral.balanceOf(address(executor)),33e18);
            assertEq(collateral.balanceOf(address(credit)),10000e18); assertEq(collateral.allowance(address(executor),address(kyber)),0);
        }
    }
    function test_kyber_callback_reentry_is_blocked() public { kyber.setMode(3); _execute(_kyberRequest(_execution())); }
    function test_kyber_amount_changed_by_protocol_never_uses_old_collateral() public {
        collateral.transfer(address(executor),1000e18); credit.configure(0,100e18,119e18);
        vm.expectRevert(); _execute(_kyberRequest(_execution())); assertEq(collateral.balanceOf(address(executor)),1000e18);
    }
    function test_kyber_code_pin_and_owner_only_configuration() public {
        vm.prank(OPERATOR); vm.expectRevert(); executor.setKyberRouter(address(kyber),address(kyber).codehash);
        vm.prank(fixtureOwner); vm.expectRevert(); executor.setKyberRouter(address(kyber),bytes32(uint256(1)));
        vm.etch(address(kyber),hex"00"); vm.expectRevert(); _execute(_kyberRequest(_execution()));
    }
    function test_kyber_rejects_tokens_recipient_permit_fees_flags_and_distribution() public {
        for (uint256 mutation; mutation<12; ++mutation) {
            IKyberRouter.Execution memory e = _execution();
            if (mutation==0) e.desc.srcToken=address(asset);
            if (mutation==1) e.desc.dstToken=address(collateral);
            if (mutation==2) e.desc.dstReceiver=borrower_A;
            if (mutation==3) e.desc.permit=hex"01";
            if (mutation==4) e.desc.flags=1;
            if (mutation==5) e.desc.flags=2;
            if (mutation==6) e.desc.flags=32;
            if (mutation==7) e.desc.feeAmounts=new uint256[](1);
            if (mutation==8) e.desc.srcAmounts[0]-=1;
            if (mutation==9) e.desc.minReturnAmount=1;
            if (mutation==10) e.callTarget=address(executor);
            if (mutation==11) e.desc.srcReceivers[0]=address(executor);
            LiquidationExecutor.Request memory r = _kyberRequest(e);
            vm.expectRevert(); _execute(r);
        }
    }
    function test_kyber_rejects_wrong_target_selector_trailing_data_and_disabled_router() public {
        IKyberRouter.Execution memory e = _execution();
        LiquidationExecutor.Request memory r = _kyberRequest(e);
        r.path=abi.encode(bytes4(0x4b594252),address(router),abi.encodeCall(IKyberRouter.swap,(e)));
        vm.expectRevert(); _execute(r);
        r.path=abi.encode(bytes4(0x4b594252),address(kyber),abi.encodeWithSelector(bytes4(0),e));
        vm.expectRevert(); _execute(r);
        r=_kyberRequest(e); r.path=bytes.concat(r.path,hex"00"); vm.expectRevert(); _execute(r);
        vm.prank(fixtureOwner); executor.setKyberRouter(address(0),bytes32(0));
        vm.expectRevert(); _execute(_kyberRequest(e));
    }
}
