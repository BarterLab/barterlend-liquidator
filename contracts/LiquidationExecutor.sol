// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

import { KyberRoute } from "./KyberRoute.sol";

import { IExecutorV4 } from "./IExecutorV4.sol";

interface IExecutorCreditPool {
    function version() external view returns (uint256);
    function asset() external view returns (address);
    function collateral() external view returns (address);
    function liquidate(uint64 market, address borrower, uint128 amount, uint256 minDiscount, uint256 deadline, bytes calldata data) external;
}

interface IExecutorLendingFactory {
    function poolVersion() external view returns (uint256);
    function isPool(address pool) external view returns (bool);
}

interface IExecutorRouter02 {
    struct ExactInputParams { bytes path; address recipient; uint256 amountIn; uint256 amountOutMinimum; }
    function exactInput(ExactInputParams calldata params) external payable returns (uint256 amountOut);
    function factory() external view returns (address);
    function WETH9() external view returns (address);
}

interface IExecutorV3Factory {
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
    function feeAmountTickSpacing(uint24 fee) external view returns (int24);
}

interface IExecutorV3Pool {
    function factory() external view returns (address);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
}

/// @notice Standard ERC20 tokens and factory-authenticated pools; profit remains after repayment.
contract LiquidationExecutor {
    using SafeERC20 for IERC20;

    struct Request {
        address pool;
        uint64 market;
        address borrower;
        uint128 requestedDebt;
        uint256 minDiscountOut;
        uint256 deadline;
        bytes path;
        uint256 minAssetProfit;
        uint256 minSwapOutput;
        bytes32 planFingerprint;
    }
    struct PoolConfig { address asset; address collateral; bool recognized; }
    enum Phase { Idle, AwaitingCallback, InCallback, Repayment, Settling, AwaitingV4Unlock, InV4Unlock }
    struct Context {
        address pool;
        address asset;
        address collateral;
        uint128 requestedDebt;
        uint128 actualRepay;
        uint128 collateralSeized;
        uint256 assetBefore;
        uint256 collateralBefore;
        uint256 assetBeforePull;
        uint256 poolAssetBeforePull;
        uint256 collateralSpent;
        uint256 minAssetProfit;
        uint256 minSwapOutput;
        bytes path;
        bytes32 fingerprint;
    }

    address public kyberRouter;
    bytes32 public kyberRouterCodeHash;
    event KyberRouterConfigured(address indexed router, bytes32 codeHash);

    address public v4Manager;
    mapping(bytes32 => bool) public allowedV4Pools;

    address public owner;
    address public pendingOwner;
    address public immutable router;
    address public immutable factory;
    address public immutable wrappedNative;
    mapping(address => bool) public operators;
    mapping(address => bool) public verifiedTokens;
    address[] private _lendingFactories;
    mapping(address => bytes32) public lendingFactoryCodeHashes;
    mapping(address => bool) public intermediates;
    mapping(uint24 => bool) public allowedFees;
    Phase public phase;
    Context private context;

    error Unauthorized();
    error Busy();
    error InvalidAddress();
    error InvalidRequest();
    error Expired();
    error UnsupportedPool();
    error UnsupportedToken();
    error InvalidRoute();
    error UnexpectedCallback();
    error CallbackAmountsMismatch();
    error MissingCallback();
    error OldBalanceSpent();
    error InsufficientProfit();
    error RepaymentMismatch();

    event V4ManagerConfigured(address indexed manager);
    event V4PoolSet(bytes32 indexed id, bool enabled);
    event IntermediateRetained(address indexed token, uint256 amount);

    event OperatorSet(address indexed operator, bool enabled);
    event TokenVerified(address indexed token, bool enabled);
    event LendingFactoryBound(address indexed lendingFactory, bytes32 codeHash);
    event IntermediateSet(address indexed token, bool enabled);
    event FeeSet(uint24 fee, bool enabled);
    event OwnershipTransferStarted(address indexed owner, address indexed pendingOwner);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event Withdrawn(address indexed token, address indexed recipient, uint256 amount);
    event LiquidationExecuted(address indexed pool, address indexed borrower, uint64 indexed market,
        bytes32 planFingerprint, uint128 actualRepay, uint128 collateralSeized,
        uint256 collateralSpent, uint256 assetProfit);

    modifier onlyOwnerIdle() {
        if (msg.sender != owner) revert Unauthorized();
        if (phase != Phase.Idle) revert Busy();
        _;
    }

    constructor(address owner_, address initialOperator, address router_, address factory_, address[] memory lendingFactories_) {
        if (owner_ == address(0) || initialOperator == address(0)
            || router_.code.length == 0 || factory_.code.length == 0) revert InvalidAddress();
        if (IExecutorRouter02(router_).factory() != factory_) revert InvalidRoute();
        address wrapped = IExecutorRouter02(router_).WETH9();
        if (wrapped.code.length == 0) revert InvalidAddress();
        if (lendingFactories_.length == 0 || lendingFactories_.length > 8) revert InvalidAddress();
        for (uint256 i; i < lendingFactories_.length; ++i) {
            address lendingFactory = lendingFactories_[i];
            if (lendingFactory.code.length == 0 || lendingFactoryCodeHashes[lendingFactory] != bytes32(0)
                || IExecutorLendingFactory(lendingFactory).poolVersion() != 5) revert UnsupportedPool();
            bytes32 codeHash = lendingFactory.codehash;
            _lendingFactories.push(lendingFactory);
            lendingFactoryCodeHashes[lendingFactory] = codeHash;
            emit LendingFactoryBound(lendingFactory, codeHash);
        }
        owner = owner_;
        operators[initialOperator] = true;
        router = router_;
        factory = factory_;
        wrappedNative = wrapped;
        emit OwnershipTransferred(address(0), owner_);
        emit OperatorSet(initialOperator, true);
    }

    /// @notice Fixed at deployment; adding a new trusted factory requires a new executor.
    function lendingFactories() external view returns (address[] memory) { return _lendingFactories; }

    /// @notice Membership is queried live, so newly created pools need no executor configuration.
    function isPool(address pool) public view returns (bool) {
        if (pool.code.length == 0) return false;
        for (uint256 i; i < _lendingFactories.length; ++i) {
            address lendingFactory = _lendingFactories[i];
            if (lendingFactory.codehash != lendingFactoryCodeHashes[lendingFactory]) continue;
            try IExecutorLendingFactory(lendingFactory).isPool(pool) returns (bool member) {
                if (member) return true;
            } catch { }
        }
        return false;
    }

    /// @notice Current factory membership and token pair. Token permissions are checked separately.
    function poolConfig(address pool) public view returns (address asset, address collateral, bool recognized) {
        if (!isPool(pool) || IExecutorCreditPool(pool).version() != 5) return (address(0), address(0), false);
        return (IExecutorCreditPool(pool).asset(), IExecutorCreditPool(pool).collateral(), true);
    }

    /// @notice One-time binding. Replacing the manager requires a new executor.
    function configureV4(address manager) external onlyOwnerIdle {
        if (v4Manager != address(0) || manager.code.length == 0) revert InvalidAddress();
        v4Manager = manager;
        emit V4ManagerConfigured(manager);
    }

    function setV4Pool(IExecutorV4.PoolKey calldata key, bool enabled) external onlyOwnerIdle {
        if (enabled && (v4Manager == address(0) || key.currency0 == address(0)
            || key.currency0 >= key.currency1 || key.hooks != address(0)
            || key.fee >= 1_000_000 || key.tickSpacing <= 0 || key.tickSpacing > 32767
            || !verifiedTokens[key.currency0] || !verifiedTokens[key.currency1])) revert InvalidRoute();
        bytes32 id = keccak256(abi.encode(key));
        allowedV4Pools[id] = enabled;
        emit V4PoolSet(id, enabled);
    }

    function setOperator(address operator, bool enabled) external onlyOwnerIdle {
        if (operator == address(0)) revert InvalidAddress();
        operators[operator] = enabled;
        emit OperatorSet(operator, enabled);
    }

    /// @dev The owner must review each token implementation before approving it.
    function setVerifiedToken(address token, bool enabled) external onlyOwnerIdle {
        if (token.code.length == 0) revert InvalidAddress();
        verifiedTokens[token] = enabled;
        emit TokenVerified(token, enabled);
    }

    function setKyberRouter(address target, bytes32 codeHash) external onlyOwnerIdle {
        if (target == address(0)) {
            if (codeHash != bytes32(0)) revert InvalidAddress();
        } else if (target == address(this) || target.code.length == 0 || target.codehash != codeHash) revert InvalidAddress();
        kyberRouter = target;
        kyberRouterCodeHash = codeHash;
        emit KyberRouterConfigured(target, codeHash);
    }

    function setIntermediate(address token, bool enabled) external onlyOwnerIdle {
        if (token == address(0) || (enabled && !verifiedTokens[token])) revert UnsupportedToken();
        intermediates[token] = enabled;
        emit IntermediateSet(token, enabled);
    }

    function setFee(uint24 fee, bool enabled) external onlyOwnerIdle {
        if (enabled && IExecutorV3Factory(factory).feeAmountTickSpacing(fee) <= 0) revert InvalidRoute();
        allowedFees[fee] = enabled;
        emit FeeSet(fee, enabled);
    }

    function transferOwnership(address next) external onlyOwnerIdle {
        if (next == address(0)) revert InvalidAddress();
        pendingOwner = next;
        emit OwnershipTransferStarted(owner, next);
    }

    function acceptOwnership() external {
        if (phase != Phase.Idle) revert Busy();
        if (msg.sender != pendingOwner || msg.sender == address(0)) revert Unauthorized();
        address previous = owner;
        owner = msg.sender;
        pendingOwner = address(0);
        emit OwnershipTransferred(previous, msg.sender);
    }

    function withdraw(address token, address recipient, uint256 amount) external onlyOwnerIdle {
        if (token.code.length == 0 || recipient == address(0) || amount == 0) revert InvalidRequest();
        phase = Phase.Settling;
        IERC20(token).safeTransfer(recipient, amount);
        phase = Phase.Idle;
        emit Withdrawn(token, recipient, amount);
    }

    function execute(Request calldata request) external returns (uint128 actualRepay,
        uint128 collateralSeized, uint256 collateralSpent, uint256 assetProfit)
    {
        if (!operators[msg.sender]) revert Unauthorized();
        if (phase != Phase.Idle) revert Busy();
        if (block.timestamp > request.deadline) revert Expired();
        if (request.borrower == address(0) || request.requestedDebt == 0
            || request.minAssetProfit == 0 || request.minSwapOutput == 0 || request.planFingerprint == bytes32(0)) revert InvalidRequest();
        PoolConfig memory config;
        (config.asset, config.collateral, config.recognized) = poolConfig(request.pool);
        if (!config.recognized) revert UnsupportedPool();
        if (!verifiedTokens[config.asset] || !verifiedTokens[config.collateral]) revert UnsupportedToken();
        if (IExecutorCreditPool(request.pool).asset() != config.asset
            || IExecutorCreditPool(request.pool).collateral() != config.collateral) revert UnsupportedPool();
        _validatePath(config.collateral, config.asset, request.path);
        context = Context({pool: request.pool, asset: config.asset, collateral: config.collateral,
            requestedDebt: request.requestedDebt, actualRepay: 0, collateralSeized: 0,
            assetBefore: IERC20(config.asset).balanceOf(address(this)),
            collateralBefore: IERC20(config.collateral).balanceOf(address(this)),
            assetBeforePull: 0, poolAssetBeforePull: 0, collateralSpent: 0,
            minAssetProfit: request.minAssetProfit, minSwapOutput: request.minSwapOutput, path: request.path, fingerprint: request.planFingerprint});
        phase = Phase.AwaitingCallback;
        IExecutorCreditPool(request.pool).liquidate(request.market, request.borrower,
            request.requestedDebt, request.minDiscountOut, request.deadline, abi.encode(request.planFingerprint));
        if (phase != Phase.Repayment) revert MissingCallback();
        phase = Phase.Settling;
        Context memory finished = context;
        uint256 afterAsset = IERC20(config.asset).balanceOf(address(this));
        if (finished.assetBeforePull < afterAsset
            || finished.assetBeforePull - afterAsset != finished.actualRepay
            || IERC20(config.asset).balanceOf(request.pool) != finished.poolAssetBeforePull + finished.actualRepay) revert RepaymentMismatch();
        if (afterAsset < finished.assetBefore + request.minAssetProfit) revert InsufficientProfit();
        if (IERC20(config.collateral).balanceOf(address(this)) < finished.collateralBefore) revert OldBalanceSpent();
        IERC20(config.asset).forceApprove(request.pool, 0);
        actualRepay = finished.actualRepay;
        collateralSeized = finished.collateralSeized;
        collateralSpent = finished.collateralSpent;
        assetProfit = afterAsset - finished.assetBefore;
        delete context;
        phase = Phase.Idle;
        emit LiquidationExecuted(request.pool, request.borrower, request.market,
            request.planFingerprint, actualRepay, collateralSeized, collateralSpent, assetProfit);
    }

    function liquidateCallback(uint128 actualRepay, uint128 acquired, bytes calldata data) external {
        Context storage active = context;
        if (phase != Phase.AwaitingCallback || msg.sender != active.pool
            || data.length != 32 || keccak256(data) != keccak256(abi.encode(active.fingerprint))) revert UnexpectedCallback();
        phase = Phase.InCallback;
        if (actualRepay == 0 || actualRepay > active.requestedDebt || acquired == 0) revert CallbackAmountsMismatch();
        uint256 collateralNow = IERC20(active.collateral).balanceOf(address(this));
        if (collateralNow < active.collateralBefore || collateralNow - active.collateralBefore != acquired) revert CallbackAmountsMismatch();
        active.actualRepay = actualRepay;
        active.collateralSeized = acquired;
        if (active.asset != active.collateral) {
            uint256 required = uint256(actualRepay) + active.minAssetProfit;
            if (active.minSwapOutput > required) required = active.minSwapOutput;
            if (active.path.length >= 128) {
                (address target, bytes memory callData,) = KyberRoute.decode(active.path, active.collateral, active.asset, address(this), acquired, active.minSwapOutput);
                if (target != kyberRouter || target.codehash != kyberRouterCodeHash) revert InvalidRoute();
                IERC20(active.collateral).forceApprove(target, acquired);
                (bool success,) = target.call(callData);
                if (!success) revert InvalidRoute();
                IERC20(active.collateral).forceApprove(target, 0);
            } else if (active.path.length == 47 || active.path.length == 73) {
                phase = Phase.AwaitingV4Unlock;
                IExecutorV4(v4Manager).unlock(abi.encode(active.fingerprint));
                if (phase != Phase.InCallback) revert MissingCallback();
            } else {
                _swapV3(active.path, acquired, required);
            }
            uint256 afterCollateral = IERC20(active.collateral).balanceOf(address(this));
            if (afterCollateral < active.collateralBefore || afterCollateral > collateralNow) revert OldBalanceSpent();
            active.collateralSpent = collateralNow - afterCollateral;
        }
        uint256 assetNow = IERC20(active.asset).balanceOf(address(this));
        if (assetNow < active.assetBefore + actualRepay + active.minAssetProfit
            || assetNow < active.assetBefore + active.minSwapOutput) revert InsufficientProfit();
        active.assetBeforePull = assetNow;
        active.poolAssetBeforePull = IERC20(active.asset).balanceOf(active.pool);
        IERC20(active.asset).forceApprove(active.pool, actualRepay);
        phase = Phase.Repayment;
    }

    // Each V3 hop pays and receives through this executor. Router02 multihop can
    // retain a partially consumed intermediate balance, which anyone can sweep.
    function _swapV3(bytes memory path, uint256 acquired, uint256 required) private {
        address input = _addressAt(path, 0);
        address middle = _addressAt(path, 23);
        if (path.length == 43) {
            _swapV3Hop(input, middle, _feeAt(path, 20), acquired, required);
        } else {
            uint256 beforeMiddle = IERC20(middle).balanceOf(address(this));
            _swapV3Hop(input, middle, _feeAt(path, 20), acquired, 1);
            uint256 amount = IERC20(middle).balanceOf(address(this)) - beforeMiddle;
            _swapV3Hop(middle, _addressAt(path, 46), _feeAt(path, 43), amount, required);
            uint256 afterMiddle = IERC20(middle).balanceOf(address(this));
            if (afterMiddle < beforeMiddle) revert OldBalanceSpent();
            if (afterMiddle > beforeMiddle) emit IntermediateRetained(middle, afterMiddle - beforeMiddle);
        }
    }

    function _swapV3Hop(address a, address b, uint24 fee, uint256 amount, uint256 minimum) private {
        IERC20(a).forceApprove(router, amount);
        IExecutorRouter02(router).exactInput(IExecutorRouter02.ExactInputParams({
            path: abi.encodePacked(a, fee, b), recipient: address(this), amountIn: amount, amountOutMinimum: minimum}));
        IERC20(a).forceApprove(router, 0);
    }

    /// @dev Only the configured PoolManager during our own unlock may call this.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        Context storage active = context;
        if (msg.sender != v4Manager || phase != Phase.AwaitingV4Unlock
            || data.length != 32 || keccak256(data) != keccak256(abi.encode(active.fingerprint))) revert UnexpectedCallback();
        phase = Phase.InV4Unlock;
        bytes memory path = active.path;
        address middle = _addressAt(path, 27);
        (uint256 spent, uint256 output) = _swapV4Hop(path, 1, active.collateralSeized);
        uint256 residual;
        if (path.length == 73) {
            uint256 firstOutput = output;
            uint256 consumed;
            (consumed, output) = _swapV4Hop(path, 27, firstOutput);
            residual = firstOutput - consumed;
        }
        IExecutorV4 manager = IExecutorV4(v4Manager);
        manager.sync(active.collateral);
        IERC20(active.collateral).safeTransfer(v4Manager, spent);
        if (manager.settle() != spent) revert CallbackAmountsMismatch();
        manager.take(active.asset, address(this), output);
        if (residual != 0) {
            uint256 beforeMiddle = IERC20(middle).balanceOf(address(this));
            manager.take(middle, address(this), residual);
            if (IERC20(middle).balanceOf(address(this)) != beforeMiddle + residual) revert CallbackAmountsMismatch();
            emit IntermediateRetained(middle, residual);
        }
        phase = Phase.InCallback;
        return bytes("");
    }

    function _swapV4Hop(bytes memory path, uint256 offset, uint256 amount) private returns (uint256 spent, uint256 output) {
        if (amount == 0 || amount > uint256(uint128(type(int128).max))) revert InvalidRequest();
        address a = _addressAt(path, offset);
        address b = _addressAt(path, offset + 26);
        IExecutorV4.PoolKey memory key = _v4Key(a, b, _feeAt(path, offset + 20), int24(_feeAt(path, offset + 23)));
        bool zeroForOne = a < b;
        int256 delta = IExecutorV4(v4Manager).swap(key, IExecutorV4.SwapParams({
            zeroForOne: zeroForOne, amountSpecified: -int256(amount),
            sqrtPriceLimitX96: zeroForOne ? uint160(4295128740) : uint160(1461446703485210103287273052203988822378723970341)}), "");
        int128 d0 = int128(delta >> 128);
        int128 d1 = int128(delta);
        int128 debit = zeroForOne ? d0 : d1;
        int128 credit = zeroForOne ? d1 : d0;
        if (debit >= 0 || credit <= 0) revert CallbackAmountsMismatch();
        spent = uint256(-int256(debit));
        output = uint256(uint128(credit));
        if (spent > amount) revert OldBalanceSpent();
    }

    function _v4Key(address a, address b, uint24 fee, int24 spacing) private pure returns (IExecutorV4.PoolKey memory) {
        return IExecutorV4.PoolKey(a < b ? a : b, a < b ? b : a, fee, spacing, address(0));
    }

    function _validatePath(address collateral, address asset, bytes memory path) private view {
        if (asset == collateral) {
            if (path.length != 0) revert InvalidRoute();
            return;
        }
        if (path.length >= 128) {
            (address target,,) = KyberRoute.decode(path, collateral, asset, address(this), 0, 1);
            if (target != kyberRouter || target.codehash != kyberRouterCodeHash) revert InvalidRoute();
            return;
        }
        if (path.length == 47 || path.length == 73) {
            _validateV4Path(collateral, asset, path);
            return;
        }
        if (path.length != 43 && path.length != 66) revert InvalidRoute();
        address first = _addressAt(path, 0);
        address middle = _addressAt(path, 23);
        if (first != collateral) revert InvalidRoute();
        if (path.length == 43) {
            if (middle != asset) revert InvalidRoute();
            _validateHop(first, middle, _feeAt(path, 20));
        } else {
            address last = _addressAt(path, 46);
            if (last != asset || middle == collateral || middle == asset
                || !intermediates[middle] || !verifiedTokens[middle]) revert InvalidRoute();
            _validateHop(first, middle, _feeAt(path, 20));
            _validateHop(middle, last, _feeAt(path, 43));
        }
    }

    function _validateV4Path(address collateral, address asset, bytes memory path) private view {
        if (v4Manager == address(0) || path[0] != 0x04 || _addressAt(path, 1) != collateral) revert InvalidRoute();
        address a = collateral;
        for (uint256 offset = 1; offset + 26 < path.length; offset += 26) {
            address b = _addressAt(path, offset + 26);
            if (b == address(0) || b == a || !verifiedTokens[b]) revert InvalidRoute();
            IExecutorV4.PoolKey memory key = _v4Key(a, b, _feeAt(path, offset + 20), int24(_feeAt(path, offset + 23)));
            if (!allowedV4Pools[keccak256(abi.encode(key))]) revert InvalidRoute();
            bool last = offset + 46 == path.length;
            if (last) { if (b != asset) revert InvalidRoute(); }
            else if (b == collateral || b == asset || !intermediates[b]) revert InvalidRoute();
            a = b;
        }
    }

    function _validateHop(address a, address b, uint24 fee) private view {
        if (a == address(0) || b == address(0) || a == b || !allowedFees[fee]) revert InvalidRoute();
        address pool = IExecutorV3Factory(factory).getPool(a, b, fee);
        if (pool.code.length == 0 || IExecutorV3Pool(pool).factory() != factory
            || IExecutorV3Pool(pool).fee() != fee) revert InvalidRoute();
        (address token0, address token1) = a < b ? (a, b) : (b, a);
        if (IExecutorV3Pool(pool).token0() != token0 || IExecutorV3Pool(pool).token1() != token1) revert InvalidRoute();
    }

    function _addressAt(bytes memory data, uint256 offset) private pure returns (address value) {
        assembly { value := shr(96, mload(add(add(data, 32), offset))) }
    }
    function _feeAt(bytes memory data, uint256 offset) private pure returns (uint24 value) {
        assembly { value := shr(232, mload(add(add(data, 32), offset))) }
    }
}
