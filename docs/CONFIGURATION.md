# Configuration

One external TOML, selected by `--config` or `BL_CONFIG`. RPC precedence: `--rpc-url` > `BL_RPC_URL` > file. Other settings come from the file or defaults. Signing requires `--live`; default is dry-run. JSON is accepted for test fixtures. Unknown bot-owned fields are rejected.

Use `config init` to discover and pin metadata and bytecode. Without `--pool`, trusted factories are read from the executor automatically. Use `--lending-factory` to select specific executor-trusted factories, or repeat `--pool` for a fixed list. Repeat `--intermediate` for swap bridge tokens. Networks: `monad` (143), `monad-testnet` (10143), `local` (31337, loopback). `--pool` and `--lending-factory` cannot be combined. An explicit pool list disables automatic discovery: the bot only works with that list. Discovery is read-only. It refuses overwrites and output inside Git.

`config check` validates offline. `doctor` verifies chain, roles, bytecode, per-pair quotes and gas valuation. Quotes do not prove liquidation execution: also use dry-run and integration tests.

## Monad Mainnet liquidity defaults

`config init --network monad` automatically fills V3, V4 and Kyber. `--quoter` defaults to the reviewed V3 QuoterV2; an explicit override must have matching V3 factory/wrapped-native bindings. Local/testnet generation still requires `--quoter`, keeps its existing V3-only behavior and uses no Mainnet preset.

The bundled `src/configuration/monad.json` contains public deployment addresses, expected runtime code hashes, and zero-hook ERC20 V4 keys for WETH/USDC, cbBTC/USDC and USDC/AUSD. Every generation checks the network/genesis, V3 deployments, V4 manager/quoter binding and bytecode, and Kyber router binding and bytecode again. It does not blindly pin whatever code the RPC returns. This is a bounded preset, not discovery of every V4 pool on the network. Other pairs can use available V3/Kyber routes; run `doctor` to check coverage.

Generation reads the executor's existing permissions. It includes only preset V4 pools whose keys and both tokens are approved. USDC, WMON and WETH are added as intermediate candidates only when both `verifiedTokens` and `intermediates` permit them. Explicit `--intermediate` values must already be approved. No token, intermediate, fee or V4-pool approval transaction is sent. Missing V4/Kyber bindings or zero approved preset V4 pools fail generation; it never silently saves a V3-only Mainnet config. CLI error causes `monad_config_v4_manager_not_configured`, `monad_config_kyber_not_configured`, `monad_config_no_approved_v4_pools` and `monad_config_intermediate_not_approved` identify the missing setup.

Kyber defaults to the public aggregator endpoint, client ID `barterlend-liquidator`, 3 requests/s and a 30-second quote TTL. No API call or API-key read is needed during config generation, so an API outage cannot prevent generation. If your gateway requires a key, set `kyber.api_key_env` to the external environment-variable name afterward; never put the key in TOML.

Mainnet generation uses a 4,000,000 gas limit (15% estimate margin) and derives fee/session caps from that limit and twice the observed gas price. Local/testnet retain 2,000,000. Review these spending caps before live execution; existing saved configs are not modified. Quote coverage, gas valuation and actual execution still need `doctor` and dry-run checks.

## Automatic pools

`config init` without `--pool` pins the factories returned by executor `lendingFactories()` and their predecessors. Optional `--lending-factory ADDRESS` selects discovery sources; every selected factory and predecessor must also be trusted by the executor. At startup the bot enumerates CREATE addresses and filters them through `isPool`; it then checks new creation nonces every 60 seconds. It does not replay historical blocks. New pools start independent synchronizers without restarting existing pools or the transaction worker. The same RPC budget covers discovery.

The factory must be the reviewed CREATE-based CreditPoolFactory v5. Code, version and predecessor are checked at a finalized block. Other deployment mechanisms, such as CREATE2 factories, need a different discovery implementation.

The executor queries factory `isPool(pool)` on every liquidation; there is no per-pool allowlist or `setPool`. Tokens still require `setVerifiedToken` owner review. For an unverified token, the bot logs `pool_discovered` with `token_approval_required` and retries later. Admitting a pool adds its token pair to the in-memory route configuration; it does not create DEX liquidity, approve V4 pool keys or add an oracle for gas valuation. These remain explicit settings. `doctor` refreshes the catalog before checking coverage.

A fixed-list config has no `[discovery]` section. In automatic mode, `[pools]` is the initial discovered snapshot, not a filter.

Generated `[discovery]` settings: `poll_seconds` (default 60), `max_creation_nonce` (4096), and the pinned `factories` list. Limits are eight factories and 64 pools in total. Each pass inspects at most 32 creation nonces per factory and four pending pool descriptors. Discovery failures retry without stopping already tracked pools; inspect `pool_discovery_retry` and `pool_discovery_candidate_retry` logs. Each candidate has a 10-second metadata timeout; a failing candidate does not block the others. Catalog state is rebuilt after restart and is never written back to the config.

## Economics

All amounts are raw token units; large integers are quoted hex strings emitted by discovery. Basis points are integers (100 = 1%). Discovery sets the gas-price cap to twice the observed RPC price, derives the transaction cap for 4,000,000 gas on Mainnet (2,000,000 on local/testnet), reserves one capped cancellation, and sets a session budget for ten capped transactions plus reserve. Review these generated limits before live execution.

| Setting | Purpose |
| --- | --- |
| `additional_profit_asset` | Extra debt-token profit beyond gas; default zero. Review raw units when pools use different decimals. |
| `fees.gas_margin_bps` | Margin on gas estimate. |
| `fees.max_gas_limit`, `fees.max_gas_price` | Hard caps; excess skips the opportunity. |
| `fees.max_fee_native` | Maximum native fee per transaction. |
| `fees.native_reserve` | Reserved MON; must cover a bumped 21,000-gas cancellation. |
| `max_total_native_spend` | Process-session cap including pending exposure. Resets on restart. |
| `max_attempts_per_nonce` | Bounded attempts per nonce. |
| `max_paid_failures_per_state` | Paid failures per position state within this process. |
| `planner.partial_divisors` | Bounded partial sizes. |
| `v3.limits.slippage_bps` | Quote haircut, in addition to repayment/profit guards. |

Readiness conservatively requires `max_fee_native + native_reserve`, not merely a positive balance.

## RPC and state

All RPC consumers and retries share **15 requests/s** by default, 8 concurrent requests, 5 reserved control slots (at most 3 concurrent background requests), a 15-second timeout and 2 retries. Adjust `[rpc]`; fallback endpoints share the total budget. `rpc.multicall3` enables a code-hash-pinned deployment. `rpc.read_optimizations` controls native batching and block-hash batching.

Set these values in the external TOML; omitted fields keep their defaults:

```toml
[rpc]
requests_per_second = 15
max_concurrent_requests = 8
reserved_control_requests = 5
request_timeout_ms = 15000
max_retries = 2
retry_backoff_ms = 250
```

The budget is shared by pool discovery, state synchronization, quotes, simulation and transaction traffic. Changing the file requires a bot restart. `rpc_url` and `rpc_fallback_urls` are top-level settings; the RPC URL can also be overridden by `--rpc-url` or `BL_RPC_URL`.

The shared limit counts requests in flight plus requests completed during the last second. Slow responses reduce throughput; delayed dispatch does not replenish the budget. Kyber has a separate cap of 3 API requests/s, including builds and retries.

A current debt census populates RAM. Events invalidate positions; getters, oracle reads and chain time produce current snapshots. Pools synchronize independently. Restart does not replay all historical blocks. Refreshing every position on every 300-ms block is not promised.

Gas valuation uses WMON, verified exact-output Uniswap paths, or explicit `native_price_feeds`. No reliable valuation means no signature. Verify feed identity, direction, decimals and freshness; Kyber exact-input quotes are not gas oracles.

## Secrets

`BL_PRIVATE_KEY_FILE`: regular file, owned by the process UID, mode 0600; symlinks are rejected. Kubernetes copies its projected Secret into a memory-backed volume. Kyber `api_key_env` contains an environment variable name, never the key itself.

One process per operator; no distributed lock. An unexplained pending nonce blocks new sends. Resolve it by checking the chain, not by repeatedly restarting. Configuration changes require a controlled restart; SIGTERM/SIGINT stops new signing and bounds shutdown to 20 seconds.

The generated runtime polls once per second. Standard route/funding TTL is 60 seconds and the block-age limit is 200 (about one minute on Monad). Kyber defaults to 30 seconds. These are maximum accepted ages, not polling intervals: each candidate refreshes debt, oracle and route before simulation, and each send rechecks its expiry. Tighten them only after measuring preparation latency with your pool count and RPC budget; 20 Monad blocks are only about six seconds.

`config init --network monad` starts with a 4,000,000 gas cap; local/testnet use 2,000,000. Gas consumption depends on the route; complex aggregator swaps may need a higher cap. Set `fees.max_gas_limit`, `fees.max_fee_native`, the session budget and operator funding together for the routes you accept. An undersized cap skips a route; it does not authorize spending above the cap.
