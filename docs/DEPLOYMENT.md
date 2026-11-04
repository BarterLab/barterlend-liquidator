# Deployment and operation

Build the bot from a pinned source revision, mount external configuration and start in keyless dry-run mode. Enable signing explicitly after checking the executor, liquidity and spending limits. Runtime state stays in memory; no database or PVC is required.

## Setup

1. Check out the reviewed liquidator revision. Its manifest and lockfile already pin the published state SDK; Cargo fetches it from GitHub. Run `python3 scripts/check-release.py` (Python 3.11+) and `cargo test --locked`. No separate SDK checkout is required. When upgrading the SDK, update its full Git SHA and `Cargo.lock` together and repeat these checks.
2. Deploy and configure the executor as described below. Bot startup never deploys or grants permissions. Keep the owner key off the bot host.
3. Generate external configuration with `config init --network monad`; it fills V3/V4/Kyber from existing executor permissions. Review limits and run `config check`, `doctor` and a keyless dry-run. Fund the dedicated operator with MON.
4. Mount configuration read-only using `BL_CONFIG`. For HTTP probes/metrics, set `health_listen` to an explicit address such as `0.0.0.0:9090` on a private container network.
5. For execution, mount the operator's owned mode-0600 key, set `BL_PRIVATE_KEY_FILE` and append `--live`. Dry-run must have no key setting. Keep RPC/API credentials external too.

This repository contains generic build/check CI and a Dockerfile. Organization-specific release workflows, Kubernetes manifests and secret provisioning belong in the operator's deployment repository. CI never signs chain transactions.

## Executor setup

Deploy `LiquidationExecutor` with constructor arguments `(owner, operator, v3Router, v3Factory, lendingFactories)`. The last argument is a list of 1–8 reviewed CreditPoolFactory v5 addresses, including pinned predecessors when present. Their runtime code hashes are fixed at deployment; use the reviewed non-proxy factories. Every liquidation checks live `isPool(pool)` membership. New pools need no executor transaction; an additional factory requires a new executor. The deployment script reads this list from comma-separated `LENDING_FACTORIES`. The router must expose the matching `factory()` and wrapped native token through `WETH9()`.

Compile the executor without private lending sources (Foundry 1.8.4, solc 0.8.31):

```sh
forge build contracts/LiquidationExecutor.sol --skip test --skip script \
  --use /path/to/solc --offline --out /external/executor-out --cache-path /external/executor-cache
```

Using the **owner** wallet, configure the following in order:

1. `setVerifiedToken(token, true)` for each reviewed collateral, debt and intermediate token. This is an owner attestation, not automatic token verification. Fee-on-transfer and rebasing tokens are unsupported.
2. `setFee(fee, true)` for the V3 fee tiers you use; the factory must support them. Call `setIntermediate(token, true)` for each V3 intermediate token.
3. For V4, call `configureV4(poolManager)` once, then `setV4Pool((currency0, currency1, fee, tickSpacing, hooks), true)` for each pool. Sort currencies by address; only ERC20 currencies, static fees and zero hooks are supported.
4. For Kyber, call `setKyberRouter(router, runtimeCodeHash)` with the verified deployment and its `keccak256(eth_getCode)` value. Add the same pins to external configuration. Set the router and hash to zero to disable it.

The constructor enables the initial operator; `setOperator(address, enabled)` manages later changes. Only the owner can change these permissions or withdraw profits. Generate configuration after these transactions are finalized, then run `doctor` and an unsigned dry-run. The bot does not need token deposits or persistent allowances; liquidation supplies collateral through its callback.

## Container

Build from this repository alone; the SDK is fetched from its pinned Git revision:

```sh
docker build -t barterlend-liquidator .
docker run --rm --name barterlend-liquidator --read-only --cap-drop ALL --security-opt no-new-privileges \
  --ulimit core=0 --user "$(id -u):$(id -g)" \
  -v /external/config.toml:/config.toml:ro \
  -e BL_CONFIG=/config.toml barterlend-liquidator
```

For live mode mount a 0600 key owned by the selected UID, set `BL_PRIVATE_KEY_FILE`, append `--live`. Never bake config or secrets into images; build context is allowlisted.

Check a built image before releasing it:

```sh
python3 tests/runners/test-container.py barterlend-liquidator
```

This checks non-root/read-only startup, key-copy permissions, `/live`, `/ready`, `/metrics`, stdout/redaction and SIGTERM with container networking disabled and no wallet. It uses a temporary local fixture; no RPC server is required. Generic CI runs it after the source checks. Deployment automation should promote that same tested image by digest instead of rebuilding it. Actual Kubernetes admission, secret projection and access to configured RPC/providers remain rollout checks.

## Monitoring and operation

`/live` checks the control-loop heartbeat with a 180-second stall limit; retries during RPC outages keep it alive. `/ready` is 503 for incomplete/stale/blocked state or insufficient funding. Head acquisition age and cached-frame age use a monotonic clock; a new publication cannot renew old chain observations. Observations older than 30 seconds are not ready. `head_age_ms`, `frame_age_ms` and per-pool `state_age_ms` continue aging on HTTP reads. Block lag remains relative to the last observed head; it is not an estimate of unseen blocks. `/metrics` reports queue, positions, lag, liquidations, skips, dropped logs and pending status. Readiness does not prove liquidity for every pair.

Logs use a production text format: UTC RFC3339 timestamp, level, target and message fields, without ANSI colors. All levels, including `ERROR`, go to stdout for the existing container log collector. No separate log exporter is needed.

```text
2026-11-04T08:00:00.123Z  INFO barterlend_liquidator::diagnostics_io: event="health_changed" details={"event":"health_changed","status":"ready"}
```

`LOG_FILTER=info` normally, `debug` for detailed search; accepted levels are `error`, `warn`, `info`, `debug`, `trace`. Third-party HTTP debug logs remain disabled to avoid leaking credentials. Diagnostic payloads are compact JSON after `details=`; the whole line is text, not JSON. Plain tracing and span-close messages may omit `details`.

Configure your container log collector to send stdout to Loki. To inspect diagnostic payloads, for example: `{app="barterlend-liquidator"} | regexp "details=(?P<payload>\\{.*\\})$" | line_format "{{.payload}}" | json` (adapt collector labels). Logging is bounded and nonblocking; alert on dropped logs.

Monitor prolonged unreadiness, RPC lag/failures, low MON, pending nonce age, exhausted spend and invalid success evidence. Cross-restart spending control needs an independent wallet monitor.

## Stopping and updating

Stop the foreground process with Ctrl-C or send SIGTERM. For the container example, use `docker stop --time 30 barterlend-liquidator`. Allow at least 30 seconds before a supervisor forces termination; the bot bounds graceful shutdown to 20 seconds.

A transaction already broadcast can still be mined after the process exits. Before restarting, withdrawing or changing operator/executor settings, inspect the operator's pending nonce and the receipts for recent transactions. Resolve outstanding transactions first. A restart loses their in-memory details and resets session spending/failure counters; it is not a recovery method.

For an update:

1. Build and test the selected revision or image before stopping the running instance. Keep the previous binary/image and external configuration for rollback.
2. Stop the old instance and check outstanding transactions. Never overlap two live instances using the same operator.
3. Run `config check` and `doctor` with the new binary and external configuration. Configuration is not hot-reloaded. If regenerating it, write a new external file and review it before switching; `config init` refuses overwrites.
4. Start the reviewed version with `--live` only when signing is intended. Check readiness, logs, wallet balance and the newly reset session budget. Use the same stop/check sequence before rolling back.

A bot update does not deploy or reconfigure the executor. Contract changes that alter the required ABI or code identity need a separately reviewed deployment and updated configuration.

## Withdrawing profit

Successful liquidations leave profit as ERC20 debt tokens in the executor. The owner's wallet calls `withdraw(token, recipient, amount)` with an explicit nonzero recipient and an amount in the token's raw units. Stop the bot and settle outstanding transactions first; check the executor balance before withdrawal and the recipient balance afterward. Leftover collateral or intermediate tokens can be withdrawn separately; they are not counted as debt-token profit.

Keep the owner separate from the operator. The operator needs MON for liquidation gas; the owner needs MON for its configuration and withdrawal transactions. Owner transactions then use a different nonce and do not change the operator's nonce. Never send native MON to the executor: its withdrawal function only transfers ERC20 tokens.
