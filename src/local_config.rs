use std::{collections::{BTreeMap, BTreeSet}, time::Duration};
use alloy::primitives::{Address, B256, U256};
use barterlend_state::{ChainIdentity, PoolSpec, TokenDescriptor};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use crate::{economics::FeePolicy, planner::PlannerConfig, routes::V3Config, tx::rpc::validate_profile_url};

pub const MAX_SDK_LOG_CHUNK_SIZE: u64 = 5_000;

fn default_sdk_log_chunk_size() -> u64 { 1_000 }
fn default_cancellation_retry_seconds() -> u64 { 30 }
fn default_history_chunks_per_step() -> usize { 8 }

/// Shared HTTP budgets for ingestion, quotes, simulations and transaction traffic.
/// Defaults match the public rpc1 throughput/log-count limits. HTTP batches are
/// not used: every individual request and retry consumes one admission slot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RpcPolicy {
    pub requests_per_second: u32,
    pub max_concurrent_requests: usize,
    pub reserved_control_requests: usize,
    pub request_timeout_ms: u64,
    pub max_retries: usize,
    pub retry_backoff_ms: u64,
    pub max_response_bytes: usize,
    pub max_logs_per_response: usize,
    /// Native pool batches and pending checks share the same process-wide budget.
    pub read_optimizations: barterlend_state::ReadOptimizationConfig,
    /// Explicit deployment pin; no implicit public contract is selected.
    pub multicall3: Option<barterlend_state::Multicall3Config>,
}

impl Default for RpcPolicy {
    fn default() -> Self {
        Self { requests_per_second:15, max_concurrent_requests:8, reserved_control_requests:5,
            request_timeout_ms:15_000, max_retries:2, retry_backoff_ms:250,
            max_response_bytes:8 * 1024 * 1024, max_logs_per_response:10_000,
            read_optimizations:barterlend_state::ReadOptimizationConfig {
                coalesce_pending_checks:true,position_batch_size:128,..Default::default() },multicall3:None }
    }
}

impl RpcPolicy {
    pub fn validate(&self) -> Result<()> {
        self.read_optimizations.validate()?;
        if let Some(config)=&self.multicall3 { config.validate()?; }
        if self.read_optimizations.batch_log_hashes && self.multicall3.is_none() {
            return Err(eyre!("batched log hash proofs require an explicitly pinned Multicall3"));
        }
        if !(1..=10_000).contains(&self.requests_per_second)
            || !(2..=256).contains(&self.max_concurrent_requests)
            || self.reserved_control_requests == 0 || self.reserved_control_requests >= self.max_concurrent_requests
            || !(1..=300_000).contains(&self.request_timeout_ms) || self.max_retries > 8
            || self.retry_backoff_ms > 30_000 || !(1_024..=64 * 1024 * 1024).contains(&self.max_response_bytes)
            || !(1..=1_000_000).contains(&self.max_logs_per_response)
        { return Err(eyre!("invalid RPC rate/concurrency/timeout/response budgets")); }
        Ok(())
    }

    pub fn bulk_config(&self, max_log_range: u64) -> barterlend_state::rpc::RpcConfig {
        barterlend_state::rpc::RpcConfig {
            // Round up so 15 admissions cannot be squeezed inside less than 1s.
            min_request_interval:Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(self.requests_per_second.max(1)))),
            max_concurrent_requests:self.max_concurrent_requests.saturating_sub(self.reserved_control_requests),
            request_timeout:Duration::from_millis(self.request_timeout_ms),
            max_retries:self.max_retries, retry_backoff:Duration::from_millis(self.retry_backoff_ms),
            max_response_bytes:self.max_response_bytes, max_logs:self.max_logs_per_response,
            max_log_range, ..Default::default()
        }
    }

    pub fn transport_config(&self, max_log_range: u64) -> barterlend_state::rpc::RpcConfig {
        barterlend_state::rpc::RpcConfig { max_concurrent_requests:self.max_concurrent_requests,
            ..self.bulk_config(max_log_range) }
    }

    pub fn control_config(&self, max_log_range: u64) -> barterlend_state::rpc::RpcConfig {
        barterlend_state::rpc::RpcConfig { max_concurrent_requests:self.reserved_control_requests,
            ..self.bulk_config(max_log_range) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPoolManifest {
    pub spec: PoolSpec,
    pub code_hash: B256,
    pub asset: TokenDescriptor,
    pub collateral: TokenDescriptor,
    pub oracle: Address,
    pub oracle_code_hash: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalManifest {
    #[serde(default)]
    pub health_listen: Option<std::net::SocketAddr>,
    pub schema: u32,
    pub profile: String,
    pub rpc_url: String,
    #[serde(default)]
    pub rpc_fallback_urls: Vec<String>,
    #[serde(default)]
    pub rpc: RpcPolicy,
    #[serde(default)]
    pub sync: crate::sync_policy::SyncPolicy,
    #[serde(default)]
    pub recovery: crate::recovery::RecoveryPolicy,
    #[serde(default = "default_cancellation_retry_seconds")]
    pub cancellation_retry_seconds: u64,
    pub chain: ChainIdentity,
    pub executor: Address,
    pub executor_code_hash: B256,
    pub owner: Address,
    pub operator: Address,
    #[serde(default)]
    pub pools: BTreeMap<String, LocalPoolManifest>,
    #[serde(default)]
    pub discovery: Option<crate::catalog::DiscoveryConfig>,
    pub v3: V3Config,
    #[serde(default)]
    pub v4: Option<crate::routes::V4Config>,
    #[serde(default)]
    pub kyber: Option<crate::routes::kyber::KyberConfig>,
    #[serde(default)]
    pub native_price_feeds: Vec<crate::economics::NativeRatioFeedConfig>,
    pub fees: FeePolicy,
    pub planner: PlannerConfig,
    /// Additional profit above the conservative gas estimate, in raw asset units.
    /// Zero permits expected net profit >= 0; on-chain minAssetProfit still covers gas.
    #[serde(default)]
    pub additional_profit_asset: U256,
    pub max_total_native_spend: U256,
    pub max_attempts_per_nonce: usize,
    pub max_paid_failures_per_state: usize,
    /// Maximum block range per eth_getLogs request.
    #[serde(default = "default_sdk_log_chunk_size")]
    pub sdk_log_chunk_size: u64,
    #[serde(default = "default_history_chunks_per_step")]
    pub sdk_history_chunks_per_step: usize,
    pub poll_ms: u64,
    pub queue_capacity: usize,
    pub cooldown_seconds: u64,
}

impl LocalManifest {
    pub fn validate(&self) -> Result<()> {
        validate_profile_url(&self.profile, self.chain.chain_id, &self.rpc_url)?;
        self.recovery.validate()?;
        self.rpc.validate()?;
        self.sync.validate()?;
        if let Some(discovery)=&self.discovery { discovery.validate()?; }
        if self.rpc_fallback_urls.len()>7 || !(1..=86_400).contains(&self.cancellation_retry_seconds) { return Err(eyre!("invalid recovery endpoints/cancellation timeout")); }
        let mut endpoints=BTreeSet::from([self.rpc_url.as_str()]);
        for url in &self.rpc_fallback_urls {
            validate_profile_url(&self.profile,self.chain.chain_id,url)?;
            if !endpoints.insert(url.as_str()) { return Err(eyre!("duplicate RPC endpoint")); }
        }
        if self.schema != 1 || self.chain.genesis_hash.is_zero()
            || self.v3.chain != self.chain || self.executor.is_zero() || self.executor_code_hash.is_zero()
            || self.owner.is_zero() || self.operator.is_zero() || (self.pools.is_empty() && self.discovery.is_none()) || self.pools.len() > 64
            || !(1..=MAX_SDK_LOG_CHUNK_SIZE).contains(&self.sdk_log_chunk_size)
            || !(1..=128).contains(&self.sdk_history_chunks_per_step)
            || !(100..=60_000).contains(&self.poll_ms) || !(1..=crate::scheduler::MAX_QUEUE_CAPACITY).contains(&self.queue_capacity)
            || self.cooldown_seconds == 0 || self.cooldown_seconds.checked_mul(16).is_none()
            || self.max_total_native_spend.is_zero() || self.max_total_native_spend < self.fees.max_fee_native
            || self.fees.native_reserve.is_zero() || !(2..=16).contains(&self.max_attempts_per_nonce)
            || !(1..=100).contains(&self.max_paid_failures_per_state)
        { return Err(eyre!("invalid explicit manifest/identity/budgets")); }
        if let Some(kyber) = &self.kyber { kyber.validate(self.chain)?; }
        if self.native_price_feeds.len() > 64 { return Err(eyre!("too many native price feeds")); }
        if let Some(v4) = &self.v4 { v4.validate(&self.v3)?; }
        self.v3.validate()?; self.fees.validate()?; self.planner.validate()?;
        let mut addresses = BTreeSet::new();
        for (name, pool) in &self.pools {
            if name.is_empty() || !addresses.insert(pool.spec.address) || pool.spec.address.is_zero() || pool.spec.expected_version != 5
                || pool.spec.deployment_block_hash.is_none() || pool.code_hash.is_zero() || pool.oracle.is_zero() || pool.oracle_code_hash.is_zero()
                || pool.asset.address.is_zero() || pool.collateral.address.is_zero()
                || !self.v3.allowed_tokens.contains(&pool.asset.address) || !self.v3.allowed_tokens.contains(&pool.collateral.address)
            { return Err(eyre!("invalid keyed pool manifest")); }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::rpc::validate_loopback_url;
    #[test]
    fn rpc_policy_defaults_match_public_budget_and_validate_partial_overrides() {
        let policy:RpcPolicy=serde_json::from_str("{}").unwrap();policy.validate().unwrap();
        assert_eq!(policy.requests_per_second,15);
        assert_eq!(policy.read_optimizations.position_batch_size,128);
        assert!(policy.read_optimizations.coalesce_pending_checks);
        assert!(policy.multicall3.is_none()&&!policy.read_optimizations.batch_log_hashes);
        let bulk=policy.bulk_config(1_000);let control=policy.control_config(1_000);
        assert_eq!(bulk.max_log_range,1_000);assert_eq!(bulk.max_logs,10_000);
        assert_eq!(bulk.max_concurrent_requests,3);assert_eq!(control.max_concurrent_requests,5);
        assert_eq!(policy.transport_config(1_000).max_concurrent_requests,8);
        assert!(bulk.min_request_interval * 15 >= Duration::from_secs(1));
        assert_eq!(bulk.min_request_interval,control.min_request_interval);
        let override_policy:RpcPolicy=serde_json::from_str("{\"requests_per_second\":1000}").unwrap();
        assert_eq!(override_policy.bulk_config(100).min_request_interval,Duration::from_millis(1));
        for patch in [serde_json::json!({"requests_per_second":0}), serde_json::json!({"max_concurrent_requests":1}),
            serde_json::json!({"reserved_control_requests":0}), serde_json::json!({"reserved_control_requests":8}),
            serde_json::json!({"request_timeout_ms":0}), serde_json::json!({"max_retries":9}),
            serde_json::json!({"max_response_bytes":0}), serde_json::json!({"max_logs_per_response":0})]
        { assert!(serde_json::from_value::<RpcPolicy>(patch).unwrap().validate().is_err()); }
        assert!(serde_json::from_str::<RpcPolicy>("{\"requests_per_second\":15,\"unknown\":1}").is_err());
        let mut manifest:serde_json::Value=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        manifest.as_object_mut().unwrap().remove("rpc");
        assert_eq!(serde_json::from_value::<LocalManifest>(manifest).unwrap().rpc.requests_per_second,15);
    }
    #[test]
    fn optimized_reads_require_explicit_pins_and_bounded_native_calls() {
        let mut policy=RpcPolicy::default();policy.read_optimizations.batch_log_hashes=true;
        assert!(policy.validate().is_err());
        policy.multicall3=Some(barterlend_state::Multicall3Config {
            address:Address::repeat_byte(9),code_hash:B256::repeat_byte(8),
            ..barterlend_state::Multicall3Config::canonical() });
        policy.validate().unwrap();
        policy.multicall3.as_mut().unwrap().code_hash=B256::ZERO;assert!(policy.validate().is_err());
        policy.multicall3.as_mut().unwrap().code_hash=B256::repeat_byte(8);
        policy.read_optimizations.position_batch_size=513;assert!(policy.validate().is_err());
        policy.read_optimizations.position_batch_size=128;policy.read_optimizations.position_batch_gas_limit=0;
        assert!(policy.validate().is_err());
    }
    #[test]
    fn break_even_profit_is_explicit_or_defaults_to_zero_without_disabling_fee_guards() {
        let mut json: serde_json::Value = serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        let explicit: LocalManifest = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(explicit.additional_profit_asset, U256::ZERO); explicit.validate().unwrap();
        assert!(!explicit.fees.max_fee_native.is_zero()); assert!(!explicit.fees.native_reserve.is_zero());
        json.as_object_mut().unwrap().remove("additional_profit_asset");
        let defaulted: LocalManifest = serde_json::from_value(json).unwrap();
        assert_eq!(defaulted.additional_profit_asset, U256::ZERO); defaulted.validate().unwrap();
    }

    #[test]
    fn external_endpoints_are_rejected_before_any_network_action() {
        for url in ["https://example.invalid", "http://example.com:8545", "http://localhost:8545", "http://127.0.0.1@evil.example", "http://127.0.0.1:8545?secret=abc", "http://user:pass@127.0.0.1:8545"] { assert!(validate_loopback_url(url).is_err()); }
        assert!(validate_loopback_url("http://127.0.0.1:8545").is_ok());
    }
    #[test]
    fn supplied_local_manifest_is_complete_and_external_profile_is_rejected() {
        let manifest:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        manifest.validate().unwrap();
        let mut bad=manifest.clone();bad.profile="mainnet".into();assert!(bad.validate().is_err());
        let mut bad=manifest.clone();bad.chain.chain_id=143;assert!(bad.validate().is_err());
        let mut bad=manifest;bad.max_paid_failures_per_state=0;assert!(bad.validate().is_err());
    }
    #[test]
    fn testnet_profile_is_explicit_and_never_enables_mainnet_or_plain_public_http() {
        let mut manifest:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        manifest.profile="monad-testnet".into();manifest.chain.chain_id=10143;manifest.v3.chain=manifest.chain;
        manifest.rpc_url="https://testnet-rpc.monad.xyz".into();manifest.validate().unwrap();
        for url in ["http://testnet-rpc.monad.xyz", "https://user:pass@testnet-rpc.monad.xyz", "https://testnet-rpc.monad.xyz?api_key=secret", "https://testnet-rpc.monad.xyz#fragment", "https://127.0.0.1:8545", "https://localhost", "http://127.0.0.1:8545"] {
            let mut bad=manifest.clone();bad.rpc_url=url.into();assert!(bad.validate().is_err(),"{url}");
        }
        for (profile,chain_id) in [("mainnet",143),("monad-testnet",143),("monad-testnet",1),("local",10143)] {
            let mut bad=manifest.clone();bad.profile=profile.into();bad.chain.chain_id=chain_id;bad.v3.chain=bad.chain;assert!(bad.validate().is_err());
        }
        let mut bad=manifest.clone();bad.chain.genesis_hash=B256::ZERO;bad.v3.chain=bad.chain;assert!(bad.validate().is_err());
        manifest.profile="monad-testnet-proxy".into();assert!(manifest.validate().is_err());
        manifest.rpc_url="http://127.0.0.1:8545".into();manifest.validate().unwrap();
        for url in ["http://localhost:8545","http://example.com:8545","http://user:pass@127.0.0.1:8545","http://127.0.0.1:8545?secret=value"] {
            let mut bad=manifest.clone();bad.rpc_url=url.into();assert!(bad.validate().is_err());
        }
    }
    #[test]
    fn invalid_scheduler_or_cancellation_limits_fail_in_manifest_validation() {
        let base:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        let mut manifest=base.clone();manifest.queue_capacity=crate::scheduler::MAX_QUEUE_CAPACITY+1;assert!(manifest.validate().is_err());
        let mut manifest=base.clone();manifest.cooldown_seconds=u64::MAX;assert!(manifest.validate().is_err());
        let mut manifest=base;manifest.max_attempts_per_nonce=1;assert!(manifest.validate().is_err());
        manifest.max_attempts_per_nonce=2;assert!(manifest.validate().is_ok());
    }
    #[test]
    fn optional_sdk_log_chunk_size_defaults_is_bounded_and_round_trips() {
        let original:serde_json::Value=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        assert!(original.get("sdk_log_chunk_size").is_none());
        let default:LocalManifest=serde_json::from_value(original.clone()).unwrap();
        assert_eq!(default.sdk_log_chunk_size,1_000);default.validate().unwrap();
        for chunk_size in [1,100,MAX_SDK_LOG_CHUNK_SIZE] {
            let mut input=original.clone();input["sdk_log_chunk_size"]=serde_json::json!(chunk_size);
            let manifest:LocalManifest=serde_json::from_value(input).unwrap();manifest.validate().unwrap();
            let restored:LocalManifest=serde_json::from_value(serde_json::to_value(&manifest).unwrap()).unwrap();
            assert_eq!(restored.sdk_log_chunk_size,chunk_size);assert_eq!(restored.pools.keys().collect::<Vec<_>>(),default.pools.keys().collect::<Vec<_>>());
            for (name,pool) in &restored.pools { assert_eq!(pool.spec,default.pools[name].spec); }
        }
        for chunk_size in [0,MAX_SDK_LOG_CHUNK_SIZE+1,u64::MAX] {
            let mut input=original.clone();input["sdk_log_chunk_size"]=serde_json::json!(chunk_size);
            let manifest:LocalManifest=serde_json::from_value(input).unwrap();assert!(manifest.validate().is_err());
        }
        for invalid in [serde_json::json!(-1),serde_json::json!(1.5),serde_json::json!("5000"),serde_json::Value::Null] {
            let mut input=original.clone();input["sdk_log_chunk_size"]=invalid;
            assert!(serde_json::from_value::<LocalManifest>(input).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn obsolete_runtime_path_is_not_used_for_writes() {
        use std::os::unix::fs::symlink;
        let root=tempfile::tempdir().unwrap();let outside=tempfile::tempdir().unwrap();
        let manifest:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
        for mode in ["live","dry-run"] {
            let mode_path=root.path().join(mode);
            symlink(outside.path(),&mode_path).unwrap();assert!(manifest.validate().is_ok());
            std::fs::remove_file(&mode_path).unwrap();std::fs::create_dir(&mode_path).unwrap();
            let chain_path=mode_path.join(format!("{}-{}",manifest.chain.chain_id,manifest.chain.genesis_hash));
            symlink(outside.path(),&chain_path).unwrap();assert!(manifest.validate().is_ok());
            std::fs::remove_file(chain_path).unwrap();
        }
        assert!(manifest.validate().is_ok());assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(),0);
    }
}
