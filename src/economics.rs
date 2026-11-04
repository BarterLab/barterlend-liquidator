//! MON cost uses the signed gas limit and a verified fresh conversion source.

use std::{collections::VecDeque, sync::Arc};
use tokio::sync::Mutex;

use alloy::{primitives::{Address, B256, U256, U512, keccak256}, sol};
use async_trait::async_trait;
use barterlend_state::{BlockRef, ChainIdentity, HttpReader, StateReader, TokenDescriptor, abi::IPriceOracle};
use serde::{Deserialize, Serialize};

use crate::routes::{ExactOutputQuote, ExactOutputRequest, QuoteValidity, RouteError, RouteKind, RouteProvider, V3RouteProvider};

#[derive(Debug, thiserror::Error)]
pub enum EconomicsError {
    #[error("invalid economics configuration: {0}")]
    InvalidConfig(String),
    #[error("fee budget: {0}")]
    Budget(String),
    #[error("insufficient native balance")]
    InsufficientNative,
    #[error("native cost is unavailable: {0}")]
    CostUnavailable(String),
    #[error("economics arithmetic: {0}")]
    Arithmetic(String),
    #[error("route: {0}")]
    Route(#[from] RouteError),
    #[error("RPC: {0}")]
    Rpc(#[from] barterlend_state::Error),
}

pub type EconomicsResult<T> = Result<T, EconomicsError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeePolicy {
    pub gas_margin_bps: u32,
    pub max_gas_limit: u64,
    pub max_gas_price: U256,
    pub max_fee_native: U256,
    pub native_reserve: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyFeePlan {
    pub signed_gas_limit: u64,
    pub gas_price: U256,
    pub fee_bound_native: U256,
}

impl FeePolicy {
    /// Leave room for at least one 12.5% fee bump before signing the first attempt.
    pub fn cancellation_headroom(&self, price: U256) -> EconomicsResult<()> {
        let bumped = cancellation_price(price)?;
        if bumped > self.max_gas_price || charged_native(21_000, bumped)? > self.native_reserve {
            return Err(EconomicsError::Budget("initial price leaves no funded cancellation headroom".into()));
        }
        Ok(())
    }
    pub fn validate(&self) -> EconomicsResult<()> {
        if self.gas_margin_bps > 10_000 || self.max_gas_limit == 0 || self.max_gas_limit > 30_000_000
            || self.max_gas_price.is_zero() || self.max_fee_native.is_zero()
        {
            return Err(EconomicsError::InvalidConfig("gas margin <=10000 bps, gas limit 1..=30000000 and positive fee caps required".into()));
        }
        Ok(())
    }

    /// Cap for the initial estimation probe, not a final signed limit. Preserve the reserve
    /// even on RPC implementations which reject a caller-supplied gas cap above its balance.
    pub fn estimation_probe_limit(&self, gas_price: U256, native_balance: U256) -> EconomicsResult<u64> {
        self.validate()?;
        if gas_price.is_zero() { return Err(EconomicsError::InvalidConfig("zero gas price".into())); }
        if gas_price > self.max_gas_price { return Err(EconomicsError::Budget("gas price cap exceeded".into())); }
        let policy_cap = U256::from(self.max_gas_limit).min(self.max_fee_native / gas_price);
        if policy_cap < U256::from(21_000) {
            return Err(EconomicsError::Budget("fee policy cannot cover intrinsic gas".into()));
        }
        let spendable = native_balance.checked_sub(self.native_reserve).ok_or(EconomicsError::InsufficientNative)?;
        let cap = policy_cap.min(spendable / gas_price);
        if cap < U256::from(21_000) { return Err(EconomicsError::InsufficientNative); }
        cap.try_into().map_err(|_| EconomicsError::Arithmetic("estimation gas cap exceeds u64".into()))
    }

    pub fn plan_legacy(&self, estimated_gas: u64, gas_price: U256, native_balance: U256) -> EconomicsResult<LegacyFeePlan> {
        self.validate()?;
        self.cancellation_headroom(gas_price)?;
        if estimated_gas == 0 || gas_price.is_zero() {
            return Err(EconomicsError::InvalidConfig("zero gas estimate/price".into()));
        }
        if gas_price > self.max_gas_price { return Err(EconomicsError::Budget("gas price cap exceeded".into())); }
        let padded = mul_div_ceil(U256::from(estimated_gas), U256::from(10_000 + self.gas_margin_bps), U256::from(10_000))?;
        let signed_gas_limit: u64 = padded.try_into().map_err(|_| EconomicsError::Arithmetic("gasLimit exceeds u64".into()))?;
        if signed_gas_limit > self.max_gas_limit { return Err(EconomicsError::Budget("signed gasLimit cap exceeded".into())); }
        let fee_bound_native = charged_native(signed_gas_limit, gas_price)?;
        if fee_bound_native > self.max_fee_native { return Err(EconomicsError::Budget("native fee cap exceeded".into())); }
        let required = fee_bound_native.checked_add(self.native_reserve)
            .ok_or_else(|| EconomicsError::Arithmetic("fee plus native reserve overflow".into()))?;
        if native_balance < required { return Err(EconomicsError::InsufficientNative); }
        Ok(LegacyFeePlan { signed_gas_limit, gas_price, fee_bound_native })
    }
}

/// gasUsed is intentionally excluded: Monad charges against the signed gas limit.
pub fn cancellation_price(price: U256) -> EconomicsResult<U256> {
    if price.is_zero() { return Err(EconomicsError::Budget("zero initial gas price".into())); }
    mul_div_ceil(price, U256::from(11_250), U256::from(10_000))
}

pub fn charged_native(signed_gas_limit: u64, effective_gas_price: U256) -> EconomicsResult<U256> {
    if signed_gas_limit == 0 || effective_gas_price.is_zero() {
        return Err(EconomicsError::InvalidConfig("zero signed gasLimit/effective gas price".into()));
    }
    U256::from(signed_gas_limit).checked_mul(effective_gas_price)
        .ok_or_else(|| EconomicsError::Arithmetic("native fee overflow".into()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeCostQuality {
    WrappedIdentity { wrapped_native: Address, code_hash: B256 },
    OnchainRatio { feed: Address, code_hash: B256, updated_at: u64 },
    V3ExactOutput { quoter: Address, route_fingerprint: B256 },
    V4ExactOutput { quoter: Address, route_fingerprint: B256 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostQuote {
    pub asset: Address,
    pub native_bound: U256,
    pub cost_asset: U256,
    pub validity: QuoteValidity,
    pub quality: NativeCostQuality,
    pub fingerprint: B256,
}

impl CostQuote {
    pub fn validate(&self, asset: Address, native_bound: U256, now: &BlockRef) -> EconomicsResult<()> {
        self.validity.validate(now)?;
        if self.asset != asset || self.native_bound != native_bound || self.asset.is_zero()
            || self.native_bound.is_zero() || self.cost_asset.is_zero() || self.fingerprint != self.compute_fingerprint()
        {
            return Err(EconomicsError::CostUnavailable("cost request/fingerprint differs or is zero".into()));
        }
        Ok(())
    }

    pub fn compute_fingerprint(&self) -> B256 {
        let mut bytes = Vec::from(b"barterlend-native-cost-v1".as_slice());
        bytes.extend_from_slice(self.asset.as_slice());
        bytes.extend_from_slice(&self.native_bound.to_be_bytes::<32>());
        bytes.extend_from_slice(&self.cost_asset.to_be_bytes::<32>());
        bytes.extend_from_slice(&self.validity.as_of.number.to_be_bytes());
        bytes.extend_from_slice(self.validity.as_of.hash.as_slice());
        bytes.extend_from_slice(self.validity.as_of.parent_hash.as_slice());
        bytes.extend_from_slice(&self.validity.as_of.timestamp.to_be_bytes());
        bytes.push(match self.validity.as_of.commitment { barterlend_state::Commitment::Finalized => 0, barterlend_state::Commitment::Safe => 1, barterlend_state::Commitment::Latest => 2 });
        bytes.extend_from_slice(&self.validity.expires_at.to_be_bytes());
        bytes.extend_from_slice(&self.validity.max_age_blocks.to_be_bytes());
        match self.quality {
            NativeCostQuality::WrappedIdentity { wrapped_native, code_hash } => {
                bytes.push(0); bytes.extend_from_slice(wrapped_native.as_slice()); bytes.extend_from_slice(code_hash.as_slice());
            }
            NativeCostQuality::OnchainRatio { feed, code_hash, updated_at } => {
                bytes.push(1); bytes.extend_from_slice(feed.as_slice()); bytes.extend_from_slice(code_hash.as_slice()); bytes.extend_from_slice(&updated_at.to_be_bytes());
            }
            NativeCostQuality::V4ExactOutput { quoter, route_fingerprint } => {
                bytes.push(3); bytes.extend_from_slice(quoter.as_slice()); bytes.extend_from_slice(route_fingerprint.as_slice());
            }
            NativeCostQuality::V3ExactOutput { quoter, route_fingerprint } => {
                bytes.push(2); bytes.extend_from_slice(quoter.as_slice()); bytes.extend_from_slice(route_fingerprint.as_slice());
            }
        }
        keccak256(bytes)
    }
}

#[async_trait]
pub trait NativeCostOracle: Send + Sync {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedEconomics {
    pub signed_gas_limit: u64,
    pub gas_price: U256,
    pub fee_bound_native: U256,
    pub gas_cost_asset: U256,
    pub min_asset_profit: U256,
    pub source_quality: NativeCostQuality,
    pub cost_fingerprint: B256,
}

pub fn build_expected_economics(asset: Address, fee: LegacyFeePlan, cost: CostQuote,
    additional_profit_asset: U256, at: &BlockRef) -> EconomicsResult<ExpectedEconomics>
{
    if charged_native(fee.signed_gas_limit, fee.gas_price)? != fee.fee_bound_native {
        return Err(EconomicsError::InvalidConfig("fee bound differs from signed envelope fields".into()));
    }
    cost.validate(asset, fee.fee_bound_native, at)?;
    if cost.validity.as_of != *at {
        return Err(EconomicsError::CostUnavailable("native cost must be anchored to the same chosen target".into()));
    }
    let min_asset_profit = cost.cost_asset.checked_add(additional_profit_asset)
        .ok_or_else(|| EconomicsError::Arithmetic("asset profit bound overflow".into()))?;
    Ok(ExpectedEconomics {
        signed_gas_limit: fee.signed_gas_limit, gas_price: fee.gas_price, fee_bound_native: fee.fee_bound_native,
        gas_cost_asset: cost.cost_asset, min_asset_profit, source_quality: cost.quality, cost_fingerprint: cost.fingerprint,
    })
}

pub type V3NativeCostOracle = RouteNativeCostOracle;

pub struct RouteNativeCostOracle {
    provider: Arc<V3RouteProvider>,
    executor: Address,
    margin_bps: u32,
    cache: Mutex<NativeCostCache>,
}

const MAX_NATIVE_ROUTES: usize = 64;
const MAX_NATIVE_QUOTES: usize = 8;

#[derive(Default)]
struct NativeCostCache {
    // Cross-block entries contain paths only. All prices remain exact-anchor data.
    routes: VecDeque<((Address, Address), RouteKind, bool)>,
    quotes: VecDeque<CostQuote>,
}

impl NativeCostCache {
    fn selected(&self, asset: Address, wrapped: Address) -> Option<(RouteKind, bool)> {
        self.routes.iter().find(|(pair, _, _)| *pair == (asset, wrapped)).map(|(_, route, explicit)| (route.clone(), *explicit))
    }

    fn select(&mut self, asset: Address, wrapped: Address, route: RouteKind, explicit: bool) {
        if self.selected(asset, wrapped).is_none_or(|(previous, _)| previous != route) {
            self.quotes.retain(|quote| quote.asset != asset);
        }
        self.routes.retain(|(pair, _, _)| *pair != (asset, wrapped));
        if self.routes.len() == MAX_NATIVE_ROUTES { self.routes.pop_front(); }
        self.routes.push_back(((asset, wrapped), route, explicit));
    }

    fn forget_route(&mut self, asset: Address, wrapped: Address) {
        self.routes.retain(|(pair, _, _)| *pair != (asset, wrapped));
        self.quotes.retain(|quote| quote.asset != asset);
    }

    fn quote(&self, asset: Address, native_bound: U256, at: &BlockRef) -> Option<CostQuote> {
        self.quotes.iter().find(|quote| quote.asset == asset && quote.native_bound == native_bound
            && quote.validity.as_of == *at).cloned()
    }

    fn remember(&mut self, quote: CostQuote) {
        self.quotes.retain(|previous| previous.asset != quote.asset || previous.native_bound != quote.native_bound
            || previous.validity.as_of != quote.validity.as_of);
        if self.quotes.len() == MAX_NATIVE_QUOTES { self.quotes.pop_front(); }
        self.quotes.push_back(quote);
    }
}

impl RouteNativeCostOracle {
    pub fn new(provider: Arc<V3RouteProvider>, executor: Address, margin_bps: u32) -> EconomicsResult<Self> {
        if executor.is_zero() || margin_bps > 10_000 {
            return Err(EconomicsError::InvalidConfig("native V3 executor/margin are invalid".into()));
        }
        Ok(Self { provider, executor, margin_bps, cache: Mutex::new(NativeCostCache::default()) })
    }

    /// Chooses the configured gas-conversion path for the next preparation.
    /// This is a path hint, not a quote: every new amount/block is revalidated.
    /// Explicit paths never trigger a hidden full search if quoting fails.
    pub async fn select_route(&self, asset: Address, route: &RouteKind) -> EconomicsResult<()> {
        let wrapped = self.provider.config().wrapped_native;
        if !self.provider.configured_routes(asset, wrapped)?.contains(route) {
            return Err(EconomicsError::InvalidConfig("native path is outside configured routes".into()));
        }
        self.cache.lock().await.select(asset, wrapped, route.clone(), true);
        Ok(())
    }

    pub async fn replenishment_quote(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<ExactOutputQuote> {
        let wrapped = self.provider.config().wrapped_native;
        // Both routing versions share the pinned wrapped-native identity.
        self.provider.verify_at(at).await?;
        let request = ExactOutputRequest { token_in: asset, token_out: wrapped, amount_out: native_bound,
            executor: self.executor, at: *at };
        let selected = self.cache.lock().await.selected(asset, wrapped);
        let explicit = selected.as_ref().is_some_and(|(_, explicit)| *explicit);
        let quote = if let Some((route, _)) = selected {
            match self.provider.quote_exact_output_route(&request, &route).await {
                Ok(quote) => quote,
                Err(RouteError::Unavailable(_)) if !explicit => {
                    // A remembered path can lose liquidity or disappear. Retry the
                    // bounded configured search, never relax identity/allowlist errors.
                    self.cache.lock().await.forget_route(asset, wrapped);
                    self.provider.quote_exact_output(&request).await?
                },
                Err(error) => return Err(error.into()),
            }
        } else {
            self.provider.quote_exact_output(&request).await?
        };
        quote.validate(asset, wrapped, native_bound, self.executor, at)?;
        self.cache.lock().await.select(asset, wrapped, quote.route.clone(), explicit);
        Ok(quote)
    }
}

#[async_trait]
impl NativeCostOracle for RouteNativeCostOracle {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote> {
        if native_bound.is_zero() { return Err(EconomicsError::InvalidConfig("zero native cost bound".into())); }
        let cached = self.cache.lock().await.quote(asset, native_bound, at);
        if let Some(quote) = cached {
            // Same amount, same entire BlockRef: immutable historical data is
            // reusable, but a reorg or an endpoint identity change still rejects it.
            if self.provider.reader().identity().await? != self.provider.config().chain {
                return Err(EconomicsError::CostUnavailable("native source chain/genesis differ".into()));
            }
            self.provider.reader().ensure_canonical(at).await?;
            quote.validate(asset, native_bound, at)?;
            return Ok(quote);
        }
        let quote = self.replenishment_quote(asset, native_bound, at).await?;
        quote.validate(asset, self.provider.config().wrapped_native, native_bound, self.executor, at)?;
        let wrapped = self.provider.config().wrapped_native;
        let quality = if asset == wrapped {
            NativeCostQuality::WrappedIdentity { wrapped_native: wrapped, code_hash: self.provider.config().code_hashes.wrapped_native }
        } else if matches!(quote.route, RouteKind::V4 { .. }) {
            NativeCostQuality::V4ExactOutput { quoter: self.provider.v4_quoter().ok_or_else(|| EconomicsError::CostUnavailable("V4 quoter missing".into()))?, route_fingerprint: quote.fingerprint }
        } else {
            NativeCostQuality::V3ExactOutput { quoter: self.provider.config().quoter, route_fingerprint: quote.fingerprint }
        };
        let cost_asset = if asset == wrapped { native_bound }
            else { apply_margin(quote.maximum_amount_in, self.margin_bps)? };
        let mut result = CostQuote { asset, native_bound, cost_asset, validity: quote.validity, quality, fingerprint: B256::ZERO };
        result.fingerprint = result.compute_fingerprint();
        self.cache.lock().await.remember(result.clone());
        Ok(result)
    }
}

sol! { interface NativeMetadataRead { function decimals() external view returns (uint8); } }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrappedNativeConfig {
    pub chain: ChainIdentity,
    pub address: Address,
    pub code_hash: B256,
    pub quote_ttl_seconds: u64,
    pub max_age_blocks: u64,
}

pub struct WmonNativeCostOracle { reader: Arc<HttpReader>, config: WrappedNativeConfig }

impl WmonNativeCostOracle {
    pub fn new(reader: Arc<HttpReader>, config: WrappedNativeConfig) -> EconomicsResult<Self> {
        validate_wrapped_config(&config)?;
        Ok(Self { reader, config })
    }
}

#[async_trait]
impl NativeCostOracle for WmonNativeCostOracle {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote> {
        if asset != self.config.address || native_bound.is_zero() {
            return Err(EconomicsError::CostUnavailable("only configured wrapped native has exact 1:1 conversion".into()));
        }
        verify_wrapped(&self.reader, &self.config, at).await?;
        let mut quote = CostQuote {
            asset, native_bound, cost_asset: native_bound,
            validity: QuoteValidity::at(*at, self.config.quote_ttl_seconds, self.config.max_age_blocks)?,
            quality: NativeCostQuality::WrappedIdentity { wrapped_native: self.config.address, code_hash: self.config.code_hash }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(quote)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRatioFeedConfig {
    pub wrapped_native: WrappedNativeConfig,
    pub feed: Address,
    pub feed_code_hash: B256,
    pub asset: TokenDescriptor,
    pub stale_window_seconds: u64,
    pub margin_bps: u32,
}

/// IPriceOracle raw ratio: WMON wei to asset units, without a second decimals adjustment.
pub struct RatioNativeCostOracle { reader: Arc<HttpReader>, config: NativeRatioFeedConfig }

/// Gas valuation is independent of the collateral provider. An explicitly pinned
/// ratio feed is authoritative for its asset; outages do not turn gas into zero.
pub struct ConfiguredNativeCostOracle {
    routes: RouteNativeCostOracle,
    feeds: std::collections::BTreeMap<Address, RatioNativeCostOracle>,
}
impl ConfiguredNativeCostOracle {
    pub fn new(provider: Arc<V3RouteProvider>, executor: Address, feeds: &[NativeRatioFeedConfig]) -> EconomicsResult<Self> {
        let mut sources = std::collections::BTreeMap::new();
        for feed in feeds {
            if feed.wrapped_native.chain != provider.config().chain || feed.wrapped_native.address != provider.config().wrapped_native
                || feed.wrapped_native.code_hash != provider.config().code_hashes.wrapped_native
                || !provider.config().allowed_tokens.contains(&feed.asset.address) || sources.contains_key(&feed.asset.address) {
                return Err(EconomicsError::InvalidConfig("native source identity or asset differs".into()));
            }
            sources.insert(feed.asset.address, RatioNativeCostOracle::new(provider.reader().clone(), feed.clone())?);
        }
        Ok(Self { routes: RouteNativeCostOracle::new(provider, executor, 100)?, feeds: sources })
    }
    pub fn paths(&self, asset: Address) -> EconomicsResult<Vec<RouteKind>> {
        if self.feeds.contains_key(&asset) { Ok(vec![RouteKind::SameToken]) }
        else { Ok(self.routes.provider.configured_routes(asset, self.routes.provider.config().wrapped_native)?) }
    }
    pub async fn select_route(&self, asset: Address, route: &RouteKind) -> EconomicsResult<()> {
        if self.feeds.contains_key(&asset) { Ok(()) } else { self.routes.select_route(asset, route).await }
    }
}
#[async_trait]
impl NativeCostOracle for ConfiguredNativeCostOracle {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote> {
        if let Some(feed) = self.feeds.get(&asset) { feed.quote_cost(asset, native_bound, at).await }
        else { self.routes.quote_cost(asset, native_bound, at).await }
    }
}

impl RatioNativeCostOracle {
    pub fn new(reader: Arc<HttpReader>, config: NativeRatioFeedConfig) -> EconomicsResult<Self> {
        validate_wrapped_config(&config.wrapped_native)?;
        if config.feed.is_zero() || config.feed_code_hash.is_zero() || config.asset.address.is_zero()
            || config.asset.address == config.wrapped_native.address || config.stale_window_seconds == 0
            || config.margin_bps > 10_000
        {
            return Err(EconomicsError::InvalidConfig("native ratio feed, asset, freshness and margin must be explicit".into()));
        }
        Ok(Self { reader, config })
    }
}

#[async_trait]
impl NativeCostOracle for RatioNativeCostOracle {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote> {
        if asset != self.config.asset.address || native_bound.is_zero() {
            return Err(EconomicsError::CostUnavailable("ratio asset/native amount differs".into()));
        }
        verify_wrapped(&self.reader, &self.config.wrapped_native, at).await?;
        let code = self.reader.get_code(self.config.feed, at).await?;
        if code.is_empty() || keccak256(code) != self.config.feed_code_hash {
            return Err(EconomicsError::CostUnavailable("ratio feed code differs".into()));
        }
        let source = self.reader.contract_call(self.config.feed, &IPriceOracle::sourceCall {}, at).await?;
        let target = self.reader.contract_call(self.config.feed, &IPriceOracle::targetCall {}, at).await?;
        let decimals = self.reader.contract_call(asset, &NativeMetadataRead::decimalsCall {}, at).await?;
        if source != self.config.wrapped_native.address || target != asset || decimals != self.config.asset.decimals {
            return Err(EconomicsError::CostUnavailable("ratio feed direction/token decimals differ".into()));
        }
        let price = self.reader.contract_call(self.config.feed, &IPriceOracle::getPriceCall {}, at).await?;
        let updated_at: u64 = price.updatedAt.try_into().map_err(|_| EconomicsError::CostUnavailable("ratio timestamp exceeds u64".into()))?;
        if updated_at > at.timestamp || updated_at.checked_add(self.config.stale_window_seconds)
            .is_none_or(|until| until <= at.timestamp)
        {
            return Err(EconomicsError::CostUnavailable("ratio feed is stale/future/overflowing".into()));
        }
        let base_cost = ratio_cost(native_bound, price.numerator, price.denominator)?;
        let cost_asset = apply_margin(base_cost, self.config.margin_bps)?;
        let expires_at = self.config.wrapped_native.quote_ttl_seconds.checked_add(at.timestamp)
            .ok_or_else(|| EconomicsError::Arithmetic("ratio quote expiry overflow".into()))?
            .min(updated_at + self.config.stale_window_seconds);
        let mut quote = CostQuote {
            asset, native_bound, cost_asset,
            validity: QuoteValidity { as_of: *at, expires_at, max_age_blocks: self.config.wrapped_native.max_age_blocks },
            quality: NativeCostQuality::OnchainRatio { feed: self.config.feed, code_hash: self.config.feed_code_hash, updated_at }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        self.reader.ensure_canonical(at).await?;
        Ok(quote)
    }
}

async fn verify_wrapped(reader: &HttpReader, config: &WrappedNativeConfig, at: &BlockRef) -> EconomicsResult<()> {
    if reader.identity().await? != config.chain { return Err(EconomicsError::CostUnavailable("native source chain/genesis differ".into())); }
    let code = reader.get_code(config.address, at).await?;
    let decimals = reader.contract_call(config.address, &NativeMetadataRead::decimalsCall {}, at).await?;
    if code.is_empty() || keccak256(code) != config.code_hash || decimals != 18 {
        return Err(EconomicsError::CostUnavailable("wrapped native identity/decimals differ".into()));
    }
    Ok(())
}

fn validate_wrapped_config(config: &WrappedNativeConfig) -> EconomicsResult<()> {
    if config.address.is_zero() || config.code_hash.is_zero() || config.chain.chain_id == 0
        || config.chain.genesis_hash.is_zero() || config.quote_ttl_seconds == 0 || config.quote_ttl_seconds > 3600
    {
        return Err(EconomicsError::InvalidConfig("wrapped native chain/address/code/TTL must be explicit".into()));
    }
    Ok(())
}

pub fn ratio_cost(native_bound: U256, numerator: U256, denominator: U256) -> EconomicsResult<U256> {
    if native_bound.is_zero() || numerator.is_zero() || denominator.is_zero() {
        return Err(EconomicsError::CostUnavailable("zero amount or invalid native ratio".into()));
    }
    mul_div_ceil(native_bound, numerator, denominator)
}

pub fn apply_margin(amount: U256, margin_bps: u32) -> EconomicsResult<U256> {
    if amount.is_zero() || margin_bps > 10_000 { return Err(EconomicsError::InvalidConfig("zero cost or excessive margin".into())); }
    mul_div_ceil(amount, U256::from(10_000 + margin_bps), U256::from(10_000))
}

pub fn mul_div_ceil(value: U256, numerator: U256, denominator: U256) -> EconomicsResult<U256> {
    if denominator.is_zero() { return Err(EconomicsError::Arithmetic("zero divisor".into())); }
    let product = U512::from(value) * U512::from(numerator);
    let divisor = U512::from(denominator);
    let mut quotient = product / divisor;
    if product % divisor != U512::ZERO {
        quotient = quotient.checked_add(U512::from(1)).ok_or_else(|| EconomicsError::Arithmetic("ceil overflow".into()))?;
    }
    let limbs = quotient.as_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(EconomicsError::Arithmetic("ratio result exceeds U256".into()));
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

#[cfg(test)]
mod native_cache_tests {
    use super::*;
    use barterlend_state::Commitment;

    fn anchor() -> BlockRef {
        BlockRef { number:10,hash:B256::repeat_byte(1),parent_hash:B256::repeat_byte(2),timestamp:100,commitment:Commitment::Latest }
    }

    fn quote(asset: Address, native_bound: u64, at: BlockRef) -> CostQuote {
        let mut quote=CostQuote { asset,native_bound:U256::from(native_bound),cost_asset:U256::from(native_bound*2),
            validity:QuoteValidity::at(at,30,20).unwrap(),
            quality:NativeCostQuality::V3ExactOutput { quoter:Address::repeat_byte(7),route_fingerprint:B256::repeat_byte(8) },fingerprint:B256::ZERO };
        quote.fingerprint=quote.compute_fingerprint();quote
    }

    #[test]
    fn exact_cost_cache_never_reuses_a_different_amount_asset_or_anchor() {
        let asset=Address::repeat_byte(3);let at=anchor();let original=quote(asset,100,at);
        let mut cache=NativeCostCache::default();cache.remember(original.clone());
        assert_eq!(cache.quote(asset,U256::from(100),&at),Some(original));
        assert!(cache.quote(asset,U256::from(101),&at).is_none());
        assert!(cache.quote(Address::repeat_byte(4),U256::from(100),&at).is_none());
        for changed in [BlockRef { number:11,..at },BlockRef { hash:B256::repeat_byte(9),..at },
            BlockRef { parent_hash:B256::repeat_byte(9),..at },BlockRef { timestamp:101,..at },
            BlockRef { commitment:Commitment::Finalized,..at }]
        { assert!(cache.quote(asset,U256::from(100),&changed).is_none()); }
    }

    #[test]
    fn native_quote_and_selected_path_caches_are_bounded_independently() {
        let asset=Address::repeat_byte(3);let wrapped=Address::repeat_byte(4);let at=anchor();
        let mut cache=NativeCostCache::default();
        for number in 1..=100u64 { cache.remember(quote(asset,number,at)); }
        assert_eq!(cache.quotes.len(),MAX_NATIVE_QUOTES);assert!(cache.quote(asset,U256::from(1),&at).is_none());
        assert!(cache.quote(asset,U256::from(100),&at).is_some());
        for number in 1..=100u8 { cache.select(Address::repeat_byte(number),wrapped,RouteKind::SameToken,false); }
        cache.remember(quote(asset,100,at));
        assert_eq!(cache.routes.len(),MAX_NATIVE_ROUTES);assert!(cache.selected(Address::repeat_byte(1),wrapped).is_none());
        assert_eq!(cache.selected(Address::repeat_byte(100),wrapped),Some((RouteKind::SameToken,false)));
        assert!(cache.selected(Address::repeat_byte(100),Address::repeat_byte(5)).is_none());
        cache.forget_route(Address::repeat_byte(100),wrapped);
        assert!(cache.selected(Address::repeat_byte(100),wrapped).is_none());
        assert!(cache.quote(asset,U256::from(100),&at).is_some(),"evicting another asset must not alter exact-block quote identity");
    }

    #[test]
    fn explicit_path_change_invalidates_only_that_assets_exact_quotes() {
        let asset=Address::repeat_byte(3);let other=Address::repeat_byte(5);let wrapped=Address::repeat_byte(4);let at=anchor();
        let mut cache=NativeCostCache::default();cache.select(asset,wrapped,RouteKind::SameToken,false);
        cache.remember(quote(asset,100,at));cache.remember(quote(other,100,at));
        cache.select(asset,wrapped,RouteKind::SameToken,true);
        assert!(cache.quote(asset,U256::from(100),&at).is_some(),"same path keeps exact data while becoming explicit");
        let tokens=vec![asset,wrapped];let fees=vec![3000];
        let route=RouteKind::V3 { path:crate::routes::encode_exact_input_path(&tokens,&fees).unwrap(),tokens,fees,pools:vec![Address::repeat_byte(6)] };
        cache.select(asset,wrapped,route.clone(),true);
        assert_eq!(cache.selected(asset,wrapped),Some((route,true)));
        assert!(cache.quote(asset,U256::from(100),&at).is_none());
        assert!(cache.quote(other,U256::from(100),&at).is_some());
        cache.remember(quote(asset,100,at));cache.routes.clear();
        cache.select(asset,wrapped,RouteKind::SameToken,true);
        assert!(cache.quote(asset,U256::from(100),&at).is_none(),"an evicted route must not let its retained quote escape explicit selection");
    }

}
