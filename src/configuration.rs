//! One externally stored configuration. Deployment pins are discovered once,
//! reviewed by the operator, and verified again before executing a liquidation.
use std::{collections::{BTreeMap, BTreeSet}, io::{Read, Write}, path::Path, sync::Arc};
use alloy::{primitives::{Address, B256, U256, keccak256}, sol};
use barterlend_state::{Commitment, HttpReader, PoolSpec, StateReader};
use color_eyre::eyre::{Result, eyre};
use crate::{local_config::{LocalManifest, LocalPoolManifest, RpcPolicy}, routes::{V3Config, V3CodeHashes, RouteLimits},
    economics::FeePolicy, planner::PlannerConfig, executor_abi::LiquidationExecutor};

sol! {
    interface ConfigRouter { function WETH9() external view returns (address); }
    interface ConfigFactory { function feeAmountTickSpacing(uint24 fee) external view returns (int24); }
}

pub fn load(path: &Path, rpc_override: Option<String>) -> Result<LocalManifest> {
    let mut options=std::fs::OpenOptions::new();options.read(true);
    // A FIFO/device must never block startup. Projected ConfigMap symlinks are permitted.
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NONBLOCK|libc::O_CLOEXEC); }
    let file = options.open(path).map_err(|_| eyre!("cannot open configuration file"))?;
    if !file.metadata()?.is_file() { return Err(eyre!("configuration must be a regular file")); }
    let mut bytes = Vec::new(); file.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 { return Err(eyre!("configuration exceeds size limit")); }
    // JSON is retained for generated local chain fixtures, TOML is the public format.
    let mut manifest: LocalManifest = if path.extension().is_some_and(|v| v == "json") {
        serde_json::from_slice(&bytes).map_err(|_| eyre!("invalid JSON configuration"))?
    } else {
        toml::from_str(std::str::from_utf8(&bytes).map_err(|_| eyre!("configuration is not UTF-8"))?)
            .map_err(|_| eyre!("invalid TOML configuration or unknown parameter"))?
    };
    if let Some(url) = rpc_override { manifest.rpc_url = url; }
    manifest.validate()?;
    Ok(manifest)
}

mod monad;

#[derive(Clone)]
pub struct Init {
    pub profile: String, pub rpc_url: String, pub executor: Address, pub operator: Address,
    pub pools: Vec<Address>, pub lending_factories: Vec<Address>, pub quoter: Option<Address>, pub intermediates: Vec<Address>,
}

async fn code_hash(reader: &HttpReader, address: Address, at: &barterlend_state::BlockRef) -> Result<B256> {
    let code = reader.get_code(address, at).await?;
    if address.is_zero() || code.is_empty() { return Err(eyre!("configured address has no deployed code")); }
    Ok(keccak256(code))
}

pub async fn discover(input: Init) -> Result<LocalManifest> {
    if !input.pools.is_empty() && !input.lending_factories.is_empty() { return Err(eyre!("choose either explicit pools or automatic factory discovery")); }
    let chain_id = match input.profile.as_str() { "local" => 31337, "monad-testnet" => 10143, "monad" => 143,
        _ => return Err(eyre!("unsupported network")) };
    crate::tx::rpc::validate_profile_url(&input.profile, chain_id, &input.rpc_url)?;
    if input.pools.len() > 64 || input.lending_factories.len()>8 || input.operator.is_zero() { return Err(eyre!("invalid operator or pool/factory count")); }
    let preset = if input.profile == "monad" { Some(monad::Preset::load()?) } else { None };
    if input.quoter.is_none() && preset.is_none() { return Err(eyre!("config init requires --quoter outside Monad Mainnet")); }
    let reader = Arc::new(HttpReader::new(&input.rpc_url, RpcPolicy::default().transport_config(1000))?);
    // Reuse the checked, shared-budget transport for its read-only gas-price method.
    use crate::tx::rpc::TxRpc;
    let chain = reader.identity().await?;
    if let Some(preset) = &preset { monad::require(chain == preset.chain, "monad_config_network_mismatch")?; }
    let tx_rpc=crate::tx::rpc::LocalTxRpc::for_endpoints(std::slice::from_ref(&input.rpc_url),reader.clone(),&input.profile,chain)?;
    let gas=tx_rpc.gas_price().await?;
    discover_with_reader(input, reader, preset, gas).await
}

async fn discover_with_reader(input: Init, reader: Arc<HttpReader>, preset: Option<monad::Preset>, gas: U256) -> Result<LocalManifest> {
    let chain_id = match input.profile.as_str() { "monad" => 143, "monad-testnet" => 10143, "local" => 31337, _ => return Err(eyre!("unsupported network")) };
    let quoter = input.quoter.or_else(|| preset.as_ref().map(|p| p.v3.quoter))
        .ok_or_else(|| eyre!("config init requires --quoter outside Monad Mainnet"))?;
    let max_gas_limit = if preset.is_some() { 4_000_000 } else { 2_000_000 };
    let rpc = RpcPolicy::default();
    let chain = reader.identity().await?;
    if let Some(preset) = &preset {
        monad::require(chain == preset.chain, "monad_config_network_mismatch")?;
    }
    if chain.chain_id != chain_id { return Err(eyre!("RPC network differs from selected network")); }
    let max_gas_price=gas.checked_mul(U256::from(2)).filter(|v| !v.is_zero()).ok_or_else(||eyre!("invalid observed gas price"))?;
    let max_fee_native=max_gas_price.checked_mul(U256::from(max_gas_limit)).ok_or_else(||eyre!("gas cap overflow"))?;
    let native_reserve=max_gas_price.checked_mul(U256::from(21_000)).ok_or_else(||eyre!("reserve overflow"))?;
    let max_total_native_spend=max_fee_native.checked_mul(U256::from(10)).and_then(|v|v.checked_add(native_reserve)).ok_or_else(||eyre!("session cap overflow"))?;
    let at = reader.head(Commitment::Finalized).await?;
    let automatic=input.pools.is_empty();
    let factory_addresses=if input.lending_factories.is_empty() && automatic {
        reader.contract_call(input.executor,&LiquidationExecutor::lendingFactoriesCall {},&at).await?
    } else { input.lending_factories };
    if automatic && !(1..=8).contains(&factory_addresses.len()) { return Err(eyre!("invalid executor lending factory count")); }
    let discovery=if automatic { Some(crate::catalog::pin_factories(&reader,factory_addresses,&at).await?) } else { None };
    let executor_code_hash = code_hash(&reader, input.executor, &at).await?;
    let owner = reader.contract_call(input.executor, &LiquidationExecutor::ownerCall {}, &at).await?;
    let router = reader.contract_call(input.executor, &LiquidationExecutor::routerCall {}, &at).await?;
    let factory = reader.contract_call(input.executor, &LiquidationExecutor::factoryCall {}, &at).await?;
    let wrapped_native = reader.contract_call(router, &ConfigRouter::WETH9Call {}, &at).await?;
    let mut fee_tiers=Vec::new();
    for fee in [100u32,500,3000,10000] {
        let amount=alloy::primitives::aliases::U24::from(fee);
        if reader.contract_call(factory,&ConfigFactory::feeAmountTickSpacingCall {fee:amount},&at).await? > alloy::primitives::aliases::I24::ZERO
            && reader.contract_call(input.executor,&LiquidationExecutor::allowedFeesCall {fee:amount},&at).await? {fee_tiers.push(fee);}
    }
    if fee_tiers.is_empty() {return Err(eyre!("no enabled V3 fee tiers shared by factory and executor"));}
    let mut pools = BTreeMap::new();
    let mut tokens = BTreeSet::from([wrapped_native]);
    tokens.extend(input.intermediates.iter().copied());
    for address in input.pools {
        let spec = PoolSpec { address, deployment_block: 0, expected_version: 5, deployment_block_hash: Some(chain.genesis_hash) };
        // Current debt census does not replay from the genesis pin.
        let pool = reader.describe_pool(&spec, &at).await?;
        tokens.extend([pool.asset.address, pool.collateral.address]);
        if pools.insert(address.to_string(), LocalPoolManifest { spec, code_hash: pool.code_hash, asset: pool.asset,
            collateral: pool.collateral, oracle: pool.oracle, oracle_code_hash: code_hash(&reader, pool.oracle, &at).await? }).is_some() {
            return Err(eyre!("duplicate pool"));
        }
    }
    let mut manifest = LocalManifest {
        discovery,
        schema: 1, profile: input.profile, rpc_url: input.rpc_url, rpc_fallback_urls: Vec::new(), rpc,
        sync: Default::default(), recovery: Default::default(), cancellation_retry_seconds: 30, chain,
        executor: input.executor, executor_code_hash, owner, operator: input.operator, pools,
        v3: V3Config { chain, factory, router, quoter, wrapped_native,
            code_hashes: V3CodeHashes { factory: code_hash(&reader, factory, &at).await?, router: code_hash(&reader, router, &at).await?,
                quoter: code_hash(&reader, quoter, &at).await?, wrapped_native: code_hash(&reader, wrapped_native, &at).await? },
            allowed_tokens: tokens, intermediates: input.intermediates, fee_tiers,
            pool_code_hashes: BTreeMap::new(), limits: RouteLimits::default() },
        v4: None, kyber: None, native_price_feeds: Vec::new(), fees: FeePolicy { gas_margin_bps: 1500, max_gas_limit,
            max_gas_price, max_fee_native, native_reserve },
        planner: PlannerConfig::default(), additional_profit_asset: U256::ZERO,
        max_total_native_spend, max_attempts_per_nonce: 4, max_paid_failures_per_state: 3,
        sdk_log_chunk_size: 1000, sdk_history_chunks_per_step: 8, poll_ms: 1000, queue_capacity: 10_000,
        cooldown_seconds: 5, health_listen: Some("0.0.0.0:9090".parse()?),
    };
    if manifest.discovery.is_some() {
        let mut cursor=crate::catalog::CatalogCursor::default();
        // Explicitly bounded startup census; subsequent polls inspect new nonces only.
        for page in 0..128 {
            if cursor.step(&reader,&mut manifest).await? { break; }
            if page==127 { return Err(eyre!("factory census exceeds startup page limit")); }
        }
    }
    if let Some(preset) = preset { monad::configure(&reader, &mut manifest, preset, &at).await?; }
    crate::app::preflight(&manifest, &reader).await?;
    crate::routes::SwapRouteProvider::new(reader.clone(), manifest.v3.clone())?.verify_at(&at).await?;
    reader.ensure_canonical(&at).await?;
    Ok(manifest)
}

pub fn save_new(path: &Path, config: &LocalManifest) -> Result<()> {
    if !path.is_absolute() { return Err(eyre!("configuration output must be absolute and outside Git")); }
    let parent = path.parent().ok_or_else(|| eyre!("configuration output has no parent"))?.canonicalize()?;
    if parent.ancestors().any(|p| p.join(".git").exists()) { return Err(eyre!("store configuration outside Git repositories")); }
    let mut options = std::fs::OpenOptions::new(); options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let bytes = toml::to_string_pretty(config)?;
    options.open(path)?.write_all(bytes.as_bytes())?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/configuration_tests.rs"]
mod tests;
