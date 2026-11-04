use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, eyre};
use barterlend_liquidator::{app::{RunOptions, run}, configuration, local_config::LocalManifest, operator_key::read_private_key_file};
use std::{path::PathBuf, process::ExitCode, sync::Arc};

#[derive(Parser)]
#[command(about="BarterLend liquidator. Dry-run is the default.")]
struct Cli {
    #[arg(long, alias="manifest", env="BL_CONFIG", global=true)] config: Option<PathBuf>,
    #[arg(long, env="BL_RPC_URL", global=true)] rpc_url: Option<String>,
    /// Send transactions using the explicitly configured network and operator.
    #[arg(long, aliases=["local-live", "testnet-live"], global=true)] live: bool,
    #[arg(long, env="BL_PRIVATE_KEY_FILE", global=true)] private_key_file: Option<PathBuf>,
    /// Zero runs until SIGINT/SIGTERM; counts main-loop iterations, not transactions.
    #[arg(long, default_value_t=0, global=true)] cycles: u64,
    #[command(subcommand)] command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Run,
    Config { #[command(subcommand)] action: ConfigAction },
    /// Read-only deployed identities and per-pair quote/gas-source coverage.
    Doctor,
}
#[derive(Subcommand)]
enum ConfigAction {
    Check,
    Init {
        #[arg(long, value_parser=["local","monad-testnet","monad"])] network: String,
        #[arg(long)] executor: Address,
        #[arg(long)] operator: Address,
        #[arg(long="pool", conflicts_with="lending_factories")] pools: Vec<Address>,
        /// Select an executor-trusted factory and its pinned predecessors.
        #[arg(long="lending-factory")] lending_factories: Vec<Address>,
        /// Defaults to the reviewed Monad Mainnet V3 quoter; required on other networks.
        #[arg(long)] quoter: Option<Address>,
        #[arg(long="intermediate")] intermediates: Vec<Address>,
        #[arg(long)] output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let code = match run_cli().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            barterlend_liquidator::diagnostics::emit_error("command_failed", &error, None);
            ExitCode::FAILURE
        },
    };
    let _ = barterlend_liquidator::diagnostics::flush_logs(std::time::Duration::from_secs(1)).await;
    code
}
async fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    barterlend_liquidator::server::disable_core_dumps()?;
    if let Some(Command::Config { action: ConfigAction::Init { network, executor, operator, pools, lending_factories, quoter, intermediates, output } }) = cli.command {
        if cli.live || cli.private_key_file.is_some() { return Err(eyre!("config init is read-only and keyless")); }
        let config = configuration::discover(configuration::Init { profile: network, rpc_url: cli.rpc_url.ok_or_else(|| eyre!("RPC URL is required for discovery"))?,
            executor, operator, pools, lending_factories, quoter, intermediates }).await?;
        configuration::save_new(&output, &config)?;
        barterlend_liquidator::diagnostics::emit_stdout(&serde_json::json!({"event":"config_created","chain_id":config.chain.chain_id,"pools":config.pools.len()}));
        return Ok(());
    }
    let config = configuration::load(cli.config.as_deref().ok_or_else(|| eyre!("--config or BL_CONFIG is required"))?, cli.rpc_url)?;
    match cli.command {
        Some(Command::Config { action: ConfigAction::Check }) => {
            if cli.live || cli.private_key_file.is_some() { return Err(eyre!("config check is keyless")); }
            barterlend_liquidator::diagnostics::emit_stdout(&serde_json::json!({"event":"config_valid","network":config.profile,"pools":config.pools.len(),"rpc_per_second":config.rpc.requests_per_second}));
        },
        Some(Command::Doctor) => {
            if cli.live || cli.private_key_file.is_some() { return Err(eyre!("doctor is read-only and keyless")); }
            doctor(&config).await?;
        },
        None | Some(Command::Run) => {
            if !cli.live && cli.private_key_file.is_some() { return Err(eyre!("dry-run is keyless")); }
            let signer: Option<PrivateKeySigner> = if cli.live {
                if let Some(path) = cli.private_key_file { Some(read_private_key_file(&path,config.operator)?) }
                else if config.profile == "local" {
                    let secret = zeroize::Zeroizing::new(std::env::var("BL_LIQUIDATOR_PRIVATE_KEY").map_err(|_| eyre!("operator key file required"))?);
                    Some(secret.parse().map_err(|_| eyre!("invalid local development key"))?)
                } else { return Err(eyre!("live requires a private operator key file")); }
            } else { None };
            let report = run(config, RunOptions { live:cli.live, cycles:cli.cycles }, signer).await?;
            barterlend_liquidator::diagnostics::emit_stdout(&serde_json::json!({"event":"stopped","dry_run_plans":report.dry_run_plans,
                "finalized_liquidations":report.finalized_liquidations,"skips":report.skips,"active_pending":report.observed_active_pending(),
                "pending_hash":report.pending_hash,"session_native_spent":report.native_spent,"health":report.health}));
        },
        Some(Command::Config { .. }) => unreachable!(),
    }
    Ok(())
}

async fn doctor(config: &LocalManifest) -> Result<()> {
    use barterlend_state::{StateReader, Commitment};
    use barterlend_liquidator::{routes::{SwapRouteProvider, LiquidityProvider, ExactInputRequest, kyber::KyberProvider},
        economics::{ConfiguredNativeCostOracle, NativeCostOracle}, tx::rpc::{LocalTxRpc, TxRpc}};
    let reader = Arc::new(barterlend_liquidator::app::state_reader(config)?);
    let mut discovered=config.clone();
    if discovered.discovery.is_some() {
        let mut cursor=barterlend_liquidator::catalog::CatalogCursor::default();
        for page in 0..128 {
            if cursor.step(&reader,&mut discovered).await? { break; }
            if page==127 { return Err(eyre!("factory census exceeds doctor page limit")); }
        }
    }
    let config=&discovered;
    barterlend_liquidator::app::preflight(config,&reader).await?;
    let uniswap = Arc::new(SwapRouteProvider::new(reader.clone(),config.v3.clone())?.with_v4(config.v4.clone())?);
    let native = ConfiguredNativeCostOracle::new(uniswap.clone(),config.executor,&config.native_price_feeds)?;
    let mut providers: Vec<Arc<dyn LiquidityProvider>> = vec![uniswap];
    if let Some(kyber) = &config.kyber { providers.push(Arc::new(KyberProvider::new(reader.clone(),config.chain,kyber.clone(),&config.v3.limits)?)); }
    let rpc = LocalTxRpc::for_endpoints(&barterlend_liquidator::app::rpc_endpoints(config),reader.clone(),&config.profile,config.chain)?;
    let balance = rpc.balance(config.operator).await?;
    let gas = rpc.gas_price().await?;
    let mut complete = !config.pools.is_empty();
    for pool in config.pools.values() {
        let amount = alloy::primitives::U256::from(10).checked_pow(alloy::primitives::U256::from(pool.collateral.decimals)).ok_or_else(|| eyre!("unsupported collateral decimals"))?;
        let mut successes = Vec::new();
        for provider in &providers {
            let hints = provider.candidates(pool.collateral.address,pool.asset.address)?;
            for hint in hints.into_iter().take(16) {
                let at=reader.head(Commitment::Latest).await?;
                let request=ExactInputRequest { token_in:pool.collateral.address,token_out:pool.asset.address,amount_in:amount,executor:config.executor,at };
                if tokio::time::timeout(std::time::Duration::from_secs(20),provider.quote(&request,&hint)).await.is_ok_and(|r|r.is_ok()) { successes.push(provider.name()); break; }
            }
        }
        let at=reader.head(Commitment::Latest).await?;
        let cost=native.quote_cost(pool.asset.address,gas.checked_mul(alloy::primitives::U256::from(config.fees.max_gas_limit)).ok_or_else(|| eyre!("fee overflow"))?,&at).await;
        complete &= !successes.is_empty() && cost.is_ok();
        barterlend_liquidator::diagnostics::emit_stdout(&serde_json::json!({"event":"coverage","pool":pool.spec.address,
            "collateral":pool.collateral.address,"asset":pool.asset.address,"sample_amount":amount,"providers":successes,"native_valuation_available":cost.is_ok(),"execution_verified":false}));
    }
    barterlend_liquidator::diagnostics::emit_stdout(&serde_json::json!({"event":"doctor","coverage_complete":complete,"operator_balance":balance,"gas_price":gas,
        "cancel_reserve_sufficient":config.fees.cancellation_headroom(gas).is_ok(),"pending_nonce":rpc.nonce(config.operator,true).await?,"latest_nonce":rpc.nonce(config.operator,false).await?}));
    if !complete { return Err(eyre!("coverage incomplete; inspect coverage events")); }
    Ok(())
}
