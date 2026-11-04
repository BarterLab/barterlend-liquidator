//! Explicit, hook-free ERC20 V4 pools. No pool discovery or arbitrary router calldata.
use std::sync::Arc;
use alloy::{primitives::{aliases::{I24, U24}, Address, B256, Bytes, U256, keccak256}, sol, sol_types::SolValue};
use barterlend_state::{BlockRef, HttpReader, PinnedRead, StateReader};
use serde::{Serialize, Deserialize};
use tokio::sync::Mutex;
use super::*;

sol! {
    struct V4PathKey { address intermediateCurrency; uint24 fee; int24 tickSpacing; address hooks; bytes hookData; }
    struct V4QuoteParams { address exactCurrency; V4PathKey[] path; uint128 exactAmount; }
    interface V4Quoter {
        function poolManager() external view returns (address);
        function quoteExactInput(V4QuoteParams params) external returns (uint256 amountOut, uint256 gasEstimate);
        function quoteExactOutput(V4QuoteParams params) external returns (uint256 amountIn, uint256 gasEstimate);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V4PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
}
impl V4PoolKey {
    pub fn id(&self) -> RouteResult<B256> {
        if self.currency0.is_zero() || self.currency0 >= self.currency1 || !self.hooks.is_zero()
            || self.fee >= 1_000_000 || !(1..=32767).contains(&self.tick_spacing) {
            return Err(RouteError::InvalidConfig("V4 requires sorted ERC20 currencies, no hooks, static fee and positive tick spacing".into()));
        }
        Ok(keccak256((self.currency0, self.currency1, U24::from(self.fee), I24::try_from(self.tick_spacing).unwrap(), self.hooks).abi_encode()))
    }
    fn connects(&self, a: Address, b: Address) -> bool {
        (self.currency0 == a && self.currency1 == b) || (self.currency0 == b && self.currency1 == a)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V4Config {
    pub manager: Address,
    pub quoter: Address,
    pub manager_code_hash: B256,
    pub quoter_code_hash: B256,
    pub pools: Vec<V4PoolKey>,
}
impl V4Config {
    pub fn validate(&self, common: &V3Config) -> RouteResult<()> {
        if self.manager.is_zero() || self.quoter.is_zero() || self.manager == self.quoter
            || self.manager_code_hash.is_zero() || self.quoter_code_hash.is_zero()
            || self.pools.is_empty() || self.pools.len() > 64 {
            return Err(RouteError::InvalidConfig("V4 identities and 1..=64 explicit pools required".into()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for key in &self.pools {
            if !ids.insert(key.id()?) || !common.allowed_tokens.contains(&key.currency0) || !common.allowed_tokens.contains(&key.currency1) {
                return Err(RouteError::InvalidConfig("duplicate V4 pool or currency outside token allowlist".into()));
            }
        }
        Ok(())
    }
}

pub fn encode_v4_path(tokens: &[Address], fees: &[u32], spacings: &[i32]) -> RouteResult<Bytes> {
    // Reuse the shared ERC20, hop count, fee and no-cycle checks.
    super::encode_exact_input_path(tokens, fees)?;
    if spacings.len() != fees.len() || spacings.iter().any(|s| !(1..=32767).contains(s)) {
        return Err(RouteError::InvalidConfig("V4 tick spacings must match hops and be 1..=32767".into()));
    }
    let mut path = vec![4];
    path.extend_from_slice(tokens[0].as_slice());
    for ((fee, spacing), token) in fees.iter().zip(spacings).zip(&tokens[1..]) {
        path.extend_from_slice(&fee.to_be_bytes()[1..]);
        path.extend_from_slice(&spacing.to_be_bytes()[1..]);
        path.extend_from_slice(token.as_slice());
    }
    Ok(path.into())
}
pub fn pool_ids(tokens: &[Address], fees: &[u32], spacings: &[i32]) -> RouteResult<Vec<B256>> {
    encode_v4_path(tokens, fees, spacings)?;
    tokens.windows(2).zip(fees).zip(spacings).map(|((t, &fee), &tick_spacing)| {
        V4PoolKey { currency0: t[0].min(t[1]), currency1: t[0].max(t[1]), fee, tick_spacing, hooks: Address::ZERO }.id()
    }).collect()
}

pub struct V4RouteProvider {
    reader: Arc<HttpReader>, config: V4Config, common: V3Config,
    verified: Mutex<Option<BlockRef>>,
}
impl V4RouteProvider {
    pub fn new(reader: Arc<HttpReader>, config: V4Config, common: V3Config) -> RouteResult<Self> {
        common.validate()?;
        config.validate(&common)?;
        Ok(Self { reader, config, common, verified: Mutex::new(None) })
    }
    pub fn quoter(&self) -> Address { self.config.quoter }
    pub async fn verify_at(&self, at: &BlockRef) -> RouteResult<()> {
        if self.reader.identity().await? != self.common.chain { return Err(RouteError::Identity("V4 chain/genesis differ".into())); }
        let mut cached = self.verified.lock().await;
        if cached.as_ref() == Some(at) { self.reader.ensure_canonical(at).await?; return Ok(()); }
        let values = self.reader.read_views(&[
            PinnedRead::Code { address: self.config.manager }, PinnedRead::Code { address: self.config.quoter },
            PinnedRead::call(self.config.quoter, &V4Quoter::poolManagerCall {}),
        ], at).await?;
        if values[0].is_empty() || values[1].is_empty()
            || keccak256(&values[0]) != self.config.manager_code_hash || keccak256(&values[1]) != self.config.quoter_code_hash
            || crate::abi_validation::decode_return::<V4Quoter::poolManagerCall>(&values[2])? != self.config.manager {
            return Err(RouteError::Identity("V4 manager/quoter code or binding differs".into()));
        }
        *cached = Some(*at);
        Ok(())
    }
    pub fn configured_routes(&self, a: Address, b: Address) -> RouteResult<Vec<RouteKind>> {
        let mut result = Vec::new();
        for key in &self.config.pools {
            if key.connects(a,b) { result.push(self.route(vec![a,b], &[key])?); }
        }
        for &middle in &self.common.intermediates {
            if middle == a || middle == b { continue; }
            for first in self.config.pools.iter().filter(|key| key.connects(a,middle)) {
                for second in self.config.pools.iter().filter(|key| key.connects(middle,b)) {
                    result.push(self.route(vec![a,middle,b], &[first, second])?);
                }
            }
        }
        Ok(result)
    }
    fn route(&self, tokens: Vec<Address>, keys: &[&V4PoolKey]) -> RouteResult<RouteKind> {
        let fees = keys.iter().map(|key| key.fee).collect::<Vec<_>>();
        let tick_spacings = keys.iter().map(|key| key.tick_spacing).collect::<Vec<_>>();
        Ok(RouteKind::V4 { path: encode_v4_path(&tokens, &fees, &tick_spacings)?,
            pool_ids: pool_ids(&tokens, &fees, &tick_spacings)?, tokens, fees, tick_spacings })
    }
    fn params(&self, route: &RouteKind, a: Address, b: Address, amount: U256, executor: Address, output: bool) -> RouteResult<V4QuoteParams> {
        if executor.is_zero() || amount.is_zero() || amount > U256::from(i128::MAX as u128)
            || !self.common.allowed_tokens.contains(&a) || !self.common.allowed_tokens.contains(&b)
            || !self.configured_routes(a,b)?.contains(route) {
            return Err(RouteError::InvalidConfig("V4 request exceeds limits or selected path is not configured".into()));
        }
        let RouteKind::V4 { tokens, fees, tick_spacings, .. } = route else { unreachable!() };
        // Exact-output Quoter traverses the path array backwards. Each element
        // therefore names that forward hop's INPUT currency (not its output).
        let path = fees.iter().enumerate().map(|(i, fee)| V4PathKey {
            intermediateCurrency: tokens[if output { i } else { i+1 }], fee: U24::from(*fee),
            tickSpacing: I24::try_from(tick_spacings[i]).unwrap(), hooks: Address::ZERO, hookData: Bytes::new(),
        }).collect();
        Ok(V4QuoteParams { exactCurrency: if output { b } else { a }, path, exactAmount: amount.to::<u128>() })
    }
    pub async fn quote_input(&self, request: &ExactInputRequest, route: &RouteKind) -> RouteResult<RouteQuote> {
        let params = self.params(route, request.token_in, request.token_out, request.amount_in, request.executor, false)?;
        self.verify_at(&request.at).await?;
        let result = self.reader.contract_call(self.config.quoter, &V4Quoter::quoteExactInputCall { params }, &request.at).await.map_err(quote_error)?;
        if result.amountOut.is_zero() { return Err(RouteError::Unavailable("V4 zero output".into())); }
        let mut quote = RouteQuote { token_in: request.token_in, token_out: request.token_out, amount_in: request.amount_in,
            amount_out: result.amountOut, minimum_amount_out: super::uniswap_v3::mul_div(result.amountOut,
                U256::from(10_000-self.common.limits.slippage_bps), U256::from(10_000), false)?,
            executor: request.executor, router: Some(self.config.manager), route: route.clone(),
            validity: QuoteValidity::at(request.at,self.common.limits.quote_ttl_seconds,self.common.limits.max_age_blocks)?,
            quoter_gas_estimate: result.gasEstimate, search: SearchQuality { candidates_examined:1, quote_calls:1, budget_exhausted:false }, fingerprint:B256::ZERO };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(quote)
    }
    pub async fn quote_output(&self, request: &ExactOutputRequest, route: &RouteKind) -> RouteResult<ExactOutputQuote> {
        let params = self.params(route, request.token_in, request.token_out, request.amount_out, request.executor, true)?;
        self.verify_at(&request.at).await?;
        let result = self.reader.contract_call(self.config.quoter, &V4Quoter::quoteExactOutputCall { params }, &request.at).await.map_err(quote_error)?;
        if result.amountIn.is_zero() { return Err(RouteError::Unavailable("V4 zero input".into())); }
        let mut quote = ExactOutputQuote { token_in: request.token_in, token_out: request.token_out, amount_out: request.amount_out,
            amount_in: result.amountIn, maximum_amount_in: super::uniswap_v3::mul_div(result.amountIn,
                U256::from(10_000+self.common.limits.slippage_bps), U256::from(10_000), true)?,
            executor: request.executor, router: Some(self.config.manager), route: route.clone(), reverse_path:route.exact_output_path()?,
            validity: QuoteValidity::at(request.at,self.common.limits.quote_ttl_seconds,self.common.limits.max_age_blocks)?,
            quoter_gas_estimate: result.gasEstimate, search: SearchQuality { candidates_examined:1, quote_calls:1, budget_exhausted:false }, fingerprint:B256::ZERO };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(quote)
    }
}
fn quote_error(error: barterlend_state::Error) -> RouteError {
    if super::uniswap_v3::quote_revert(&error) { RouteError::Unavailable("V4 pool/amount cannot be quoted".into()) }
    else { RouteError::Rpc(error) }
}
