use alloy::sol;

// Checked against the compiled contract ABI by an integration test.
sol! {
    #[derive(Debug)]
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

    interface LiquidationExecutor {
        function execute(Request calldata request) external returns (
            uint128 actualRepay, uint128 collateralSeized,
            uint256 collateralSpent, uint256 assetProfit
        );
        function owner() external view returns (address);
        function operators(address operator) external view returns (bool);
        function poolConfig(address pool) external view returns (address asset, address collateral, bool recognized);
        function isPool(address pool) external view returns (bool);
        function verifiedTokens(address token) external view returns (bool);
        function lendingFactories() external view returns (address[]);
        function lendingFactoryCodeHashes(address lendingFactory) external view returns (bytes32);
        function router() external view returns (address);
        function factory() external view returns (address);
        function kyberRouter() external view returns (address);
        function kyberRouterCodeHash() external view returns (bytes32);
        function v4Manager() external view returns (address);
        function allowedV4Pools(bytes32 id) external view returns (bool);
        function allowedFees(uint24 fee) external view returns (bool);
        event LiquidationExecuted(
            address indexed pool, address indexed borrower, uint64 indexed market,
            bytes32 planFingerprint, uint128 actualRepay, uint128 collateralSeized,
            uint256 collateralSpent, uint256 assetProfit
        );
    }
}
