# Uniswap V3 test artifacts

The bundle contains `UniswapV3Factory`, `UniswapV3Pool`, `QuoterV2` and `SwapRouter02` artifacts used by the local integration tests.

| Package | Version |
| --- | --- |
| `@uniswap/v3-core` | 1.0.1 |
| `@uniswap/v3-periphery` | 1.4.4 |
| `@uniswap/swap-router-contracts` | 1.3.1 |

[MANIFEST.json](MANIFEST.json) records package integrity and artifact hashes. Keep the accompanying licenses when redistributing the fixtures.

The pool init-code hash is `0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54`. The harness deploys pinned creation bytecode with constructor arguments. Recompiling these legacy contracts with the executor's compiler/settings can change CREATE2 addresses. Runtime templates contain immutable slots and cannot be compared to deployed code without accounting for them.

These files are test inputs, not network addresses or a configured deployment. See [the testing guide](../../../docs/TESTING.md) for local scenarios and artifact generation.

Sources: [V3 core](https://github.com/Uniswap/v3-core#local-deployment), [SwapRouter02](https://github.com/Uniswap/swap-router-contracts#local-deployment), [pool address calculation](https://github.com/Uniswap/v3-periphery/blob/main/contracts/libraries/PoolAddress.sol).
