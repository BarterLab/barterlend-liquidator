// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.31;

import { ERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";
import { IERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "lib/openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import { IPriceOracle } from "src/interfaces/IPriceOracle.sol";

/// @notice Test fixture only: synthetic asset and collateral minting is restricted to the owner.
contract TestnetToken is ERC20 {
    address public immutable owner;
    constructor(address owner_, string memory name_, string memory symbol_) ERC20(name_, symbol_) {
        require(owner_ != address(0), "owner");
        owner = owner_;
    }
    function mint(address recipient, uint256 amount) external {
        require(msg.sender == owner, "owner");
        _mint(recipient, amount);
    }
}

/// @notice Wrapped token backed by test MON, with a WETH9-compatible ABI.
contract TestnetWrappedMON is ERC20 {
    constructor() ERC20("Test Stack Wrapped MON", "tsWMON") {}
    receive() external payable { _mint(msg.sender, msg.value); }
    function deposit() external payable { _mint(msg.sender, msg.value); }
    function withdraw(uint256 amount) external {
        _burn(msg.sender, amount);
        (bool ok,) = msg.sender.call{value: amount}("");
        require(ok, "native transfer");
    }
}

/// @notice Controlled test oracle. Invalid observations exercise no-send scenarios.
contract TestnetOracle is IPriceOracle {
    address public immutable owner;
    address public immutable source;
    address public immutable target;
    Price private observation;
    constructor(address owner_, address source_, address target_) {
        require(owner_ != address(0) && source_ != target_ && source_.code.length != 0 && target_.code.length != 0, "addresses");
        owner = owner_; source = source_; target = target_;
        observation = Price(1e18, 1e18, block.timestamp);
    }
    function setPriceFraction(uint256 numerator, uint256 denominator) external {
        require(msg.sender == owner, "owner");
        observation = Price(numerator, denominator, block.timestamp);
    }
    function setObservation(uint256 numerator, uint256 denominator, uint256 updatedAt) external {
        require(msg.sender == owner, "owner");
        observation = Price(numerator, denominator, updatedAt);
    }
    function touch() external {
        require(msg.sender == owner, "owner");
        observation.updatedAt = block.timestamp;
    }
    function getPrice() external view returns (Price memory) { return observation; }
}

/// @notice Non-yielding Aave ABI receipt: one receipt represents one asset held by the pool.
contract TestnetAaveReceipt is ERC20 {
    address public immutable owner;
    address public immutable UNDERLYING_ASSET_ADDRESS;
    address public POOL;
    constructor(address owner_, address asset_) ERC20("Test Stack Underlying Receipt", "tsaASSET") {
        require(owner_ != address(0) && asset_.code.length != 0, "addresses");
        owner = owner_; UNDERLYING_ASSET_ADDRESS = asset_;
    }
    function setPool(address pool_) external {
        require(msg.sender == owner && POOL == address(0) && pool_.code.length != 0, "pool");
        require(TestnetAavePool(pool_).asset() == UNDERLYING_ASSET_ADDRESS && address(TestnetAavePool(pool_).receipt()) == address(this), "identity");
        POOL = pool_;
    }
    function mint(address account, uint256 amount) external { require(msg.sender == POOL, "pool"); _mint(account, amount); }
    function burn(address account, uint256 amount) external { require(msg.sender == POOL, "pool"); _burn(account, amount); }
}

/// @notice Synthetic underlying with 1:1 custody; exposes the ABI used by the pinned AaveV3Adapter.
contract TestnetAavePool {
    using SafeERC20 for IERC20;
    address public immutable asset;
    TestnetAaveReceipt public immutable receipt;
    constructor(address asset_, TestnetAaveReceipt receipt_) {
        require(asset_.code.length != 0 && address(receipt_).code.length != 0 && receipt_.UNDERLYING_ASSET_ADDRESS() == asset_, "identity");
        asset = asset_; receipt = receipt_;
    }
    function getReserveAToken(address asset_) external view returns (address) {
        return asset_ == asset ? address(receipt) : address(0);
    }
    function supply(address asset_, uint256 amount, address recipient, uint16) external {
        require(asset_ == asset && recipient != address(0) && receipt.POOL() == address(this), "identity");
        uint256 beforeBalance = IERC20(asset).balanceOf(address(this));
        IERC20(asset).safeTransferFrom(msg.sender, address(this), amount);
        require(IERC20(asset).balanceOf(address(this)) == beforeBalance + amount, "exact receipt");
        receipt.mint(recipient, amount);
    }
    function withdraw(address asset_, uint256 amount, address recipient) external returns (uint256) {
        require(asset_ == asset && recipient != address(0), "identity");
        if (amount == type(uint256).max) amount = receipt.balanceOf(msg.sender);
        receipt.burn(msg.sender, amount);
        uint256 beforeBalance = IERC20(asset).balanceOf(recipient);
        IERC20(asset).safeTransfer(recipient, amount);
        require(IERC20(asset).balanceOf(recipient) == beforeBalance + amount, "exact withdrawal");
        return amount;
    }
}

interface ITestnetV3Factory {
    function createPool(address tokenA, address tokenB, uint24 fee) external returns (address);
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
}
interface ITestnetV3Pool {
    function factory() external view returns (address);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function initialize(uint160 sqrtPriceX96) external;
    function mint(address recipient, int24 lower, int24 upper, uint128 liquidity, bytes calldata data) external returns (uint256, uint256);
    function burn(int24 lower, int24 upper, uint128 liquidity) external returns (uint256, uint256);
    function collect(address recipient, int24 lower, int24 upper, uint128 amount0Max, uint128 amount1Max) external returns (uint128, uint128);
}

/// @notice The owner funds minting through allowances; bootstrap holds LP tokens and can return them to the owner.
contract TestnetV3Bootstrap {
    using SafeERC20 for IERC20;
    address public immutable owner;
    address public immutable factory;
    address private expectedPool;
    constructor(address owner_, address factory_) {
        require(owner_ != address(0) && factory_.code.length != 0, "addresses");
        owner = owner_; factory = factory_;
    }
    function seedPool(address tokenA, address tokenB, uint24 fee, uint160 sqrtPriceX96, int24 lower, int24 upper, uint128 liquidity) external returns (address pool, uint256 amount0, uint256 amount1) {
        require(msg.sender == owner && expectedPool == address(0) && tokenA != tokenB && liquidity != 0, "owner/state");
        // Fresh-only step: the resume runner checks the existing pool and LP before sending.
        require(ITestnetV3Factory(factory).getPool(tokenA, tokenB, fee) == address(0), "already seeded");
        pool = ITestnetV3Factory(factory).createPool(tokenA, tokenB, fee);
        ITestnetV3Pool(pool).initialize(sqrtPriceX96);
        expectedPool = pool;
        (amount0, amount1) = ITestnetV3Pool(pool).mint(address(this), lower, upper, liquidity, "");
        expectedPool = address(0);
    }
    function uniswapV3MintCallback(uint256 amount0, uint256 amount1, bytes calldata) external {
        require(msg.sender == expectedPool && expectedPool != address(0), "callback");
        if (amount0 != 0) IERC20(ITestnetV3Pool(msg.sender).token0()).safeTransferFrom(owner, msg.sender, amount0);
        if (amount1 != 0) IERC20(ITestnetV3Pool(msg.sender).token1()).safeTransferFrom(owner, msg.sender, amount1);
    }
    function removeLiquidity(address pool, int24 lower, int24 upper, uint128 liquidity) external returns (uint128 amount0, uint128 amount1) {
        require(msg.sender == owner && expectedPool == address(0) && ITestnetV3Pool(pool).factory() == factory, "owner/state");
        (address token0, address token1, uint24 fee) = (ITestnetV3Pool(pool).token0(), ITestnetV3Pool(pool).token1(), ITestnetV3Pool(pool).fee());
        require(ITestnetV3Factory(factory).getPool(token0, token1, fee) == pool, "pool");
        if (liquidity != 0) ITestnetV3Pool(pool).burn(lower, upper, liquidity);
        return ITestnetV3Pool(pool).collect(owner, lower, upper, type(uint128).max, type(uint128).max);
    }
}
