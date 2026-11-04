# BarterLend liquidator

A Rust bot for unhealthy and matured BarterLend debts. It simulates each liquidation and atomically sells collateral through Uniswap V3/V4 or Kyber. Execution requires expected proceeds to cover repayment, the **signed gas limit** cost and additional profit (zero by default).

State and transaction tracking live in memory: no database or PVC. Use one process and a dedicated operator wallet. Restart resets spending/failure counters and loses pending-transaction details.

## Quickstart

Requires Rust 1.98.0 (pinned toolchain), an RPC endpoint and a [deployed, configured executor](docs/DEPLOYMENT.md#executor-setup). Cargo downloads the state SDK from its pinned GitHub revision; no separate SDK checkout is needed. Set `EXECUTOR` to your contract address and `OPERATOR` to your dedicated bot wallet address below.

```sh
cargo build --release --locked --bin barterlend-liquidator
export BL_RPC_URL='https://your-rpc-endpoint'
export EXECUTOR='<executor-address>'
export OPERATOR='<operator-wallet-address>'
./target/release/barterlend-liquidator config init \
  --network monad --executor "$EXECUTOR" --operator "$OPERATOR" \
  --output "$HOME/liquidator.toml"
export BL_CONFIG="$HOME/liquidator.toml"
./target/release/barterlend-liquidator config check
./target/release/barterlend-liquidator doctor
./target/release/barterlend-liquidator  # keyless dry-run
```

On Monad Mainnet, `config init` fills V3, V4 and Kyber from the bundled deployment profile and verifies on-chain identities. It selects already approved V4 pools and intermediate tokens; it never grants permissions. Configure the executor first. Local/testnet generation still requires `--quoter` and stays V3-only. See [configuration](docs/CONFIGURATION.md) for the preset's coverage and gas caps.

By default, the bot reads the trusted lending factories from the executor and automatically discovers their existing and newly created pools. No per-pool owner transaction is needed. Pass repeated `--pool` arguments to `config init` for a fixed list. Tokens still require owner review, and each pair needs a usable swap route and gas valuation.

Review external configuration, routes and spending limits. Store the operator private key in a file outside Git, owned by the user running the bot. Enable signing explicitly:

```sh
chmod 600 "$HOME/liquidator-operator.key"
export BL_PRIVATE_KEY_FILE="$HOME/liquidator-operator.key"
./target/release/barterlend-liquidator --live
```

Fund the **operator wallet with MON** for gas. Profit stays in the executor as debt tokens; its **owner** can withdraw to a chosen recipient. Use a separate owner wallet. Reverts still cost gas; simulation cannot eliminate execution risks.

This repository contains reusable source code, tests, generic CI and a Dockerfile. Keep environment-specific deployment files in a separate repository; supply runtime configuration and credentials externally. See [operation and upgrades](docs/DEPLOYMENT.md#stopping-and-updating) for shutdown, restart and profit withdrawal.

- [Configuration](docs/CONFIGURATION.md)
- [Liquidity and adding providers](docs/LIQUIDITY.md)
- [Docker, logs and operations](docs/DEPLOYMENT.md)
- [Tests](docs/TESTING.md)

## License

Licensed under the [MIT License](LICENSE). Third-party components retain their own licenses.
