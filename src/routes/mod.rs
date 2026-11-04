//! Bounded read-only quotes; execution requires a separate full simulation.

pub mod kyber;
pub mod registry;
pub use registry::{ProviderRegistry, LiquidityProvider, RouteCandidate};
pub mod uniswap_v3;
pub mod uniswap_v4;
pub use uniswap_v4::{V4Config, V4PoolKey, encode_v4_path};

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use async_trait::async_trait;
use barterlend_state::BlockRef;
use serde::{Deserialize, Serialize};

pub use uniswap_v3::{V3CodeHashes, V3Config, V3Identity, V3RouteProvider, SwapRouteProvider};

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("invalid route configuration: {0}")]
    InvalidConfig(String),
    #[error("route identity mismatch: {0}")]
    Identity(String),
    #[error("route is unavailable: {0}")]
    Unavailable(String),
    #[error("quote is stale: {0}")]
    Stale(String),
    #[error("route arithmetic: {0}")]
    Arithmetic(String),
    #[error("RPC: {0}")]
    Rpc(#[from] barterlend_state::Error),
}

pub type RouteResult<T> = Result<T, RouteError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteValidity {
    pub as_of: BlockRef,
    pub expires_at: u64,
    pub max_age_blocks: u64,
}

impl QuoteValidity {
    pub fn at(block: BlockRef, ttl_seconds: u64, max_age_blocks: u64) -> RouteResult<Self> {
        if ttl_seconds == 0 {
            return Err(RouteError::InvalidConfig("quote TTL must be positive".into()));
        }
        let expires_at = block.timestamp.checked_add(ttl_seconds)
            .ok_or_else(|| RouteError::Arithmetic("quote expiry overflow".into()))?;
        Ok(Self { as_of: block, expires_at, max_age_blocks })
    }

    pub fn validate(&self, now: &BlockRef) -> RouteResult<()> {
        if self.expires_at <= self.as_of.timestamp || now.number < self.as_of.number
            || now.timestamp < self.as_of.timestamp || now.timestamp >= self.expires_at
            || now.number - self.as_of.number > self.max_age_blocks
        {
            return Err(RouteError::Stale("quote block/time limits exceeded".into()));
        }
        if now.number == self.as_of.number && now.hash != self.as_of.hash {
            return Err(RouteError::Stale("quote anchor hash changed".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLimits {
    pub max_candidates: usize,
    pub max_quote_calls: usize,
    pub quote_ttl_seconds: u64,
    pub max_age_blocks: u64,
    pub slippage_bps: u32,
}

impl Default for RouteLimits {
    fn default() -> Self {
        // Monad produces about 200 blocks per minute. Keep a bounded minute
        // for shared-budget preparation; execution still enforces output/profit.
        Self { max_candidates: 32, max_quote_calls: 16, quote_ttl_seconds: 60, max_age_blocks: 200, slippage_bps: 100 }
    }
}

impl RouteLimits {
    pub fn validate(&self) -> RouteResult<()> {
        if self.max_candidates == 0 || self.max_candidates > 256 || self.max_quote_calls == 0
            || self.max_quote_calls > 256 || self.quote_ttl_seconds == 0
            || self.quote_ttl_seconds > 3600 || self.slippage_bps >= 10_000
        {
            return Err(RouteError::InvalidConfig("candidate/quote limits 1..=256, TTL 1..=3600, slippage < 10000 bps required".into()));
        }
        Ok(())
    }
}

/// The path is always forward exact-input; exact-output encoding is computed separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteKind {
    SameToken,
    Kyber { path: Bytes },
    V3 { path: Bytes, tokens: Vec<Address>, fees: Vec<u32>, pools: Vec<Address> },
    V4 { path: Bytes, tokens: Vec<Address>, fees: Vec<u32>, tick_spacings: Vec<i32>, pool_ids: Vec<B256> },
}

impl RouteKind {
    pub fn source(&self) -> &'static str {
        match self { Self::SameToken=>"same_token", Self::V3 {..}=>"uniswap_v3", Self::V4 {..}=>"uniswap_v4", Self::Kyber {..}=>"kyber" }
    }
    pub fn exact_input_path(&self) -> Bytes {
        match self { Self::SameToken => Bytes::new(), Self::V3 { path, .. } | Self::V4 { path, .. } | Self::Kyber { path } => path.clone() }
    }

    pub fn exact_output_path(&self) -> RouteResult<Bytes> {
        match self {
            Self::SameToken => Ok(Bytes::new()),
            Self::Kyber { .. } => Err(RouteError::InvalidConfig("Kyber is exact-input only".into())),
            Self::V3 { tokens, fees, .. } => encode_exact_output_path(tokens, fees),
            Self::V4 { tokens, fees, tick_spacings, .. } => encode_v4_path(
                &tokens.iter().rev().copied().collect::<Vec<_>>(),
                &fees.iter().rev().copied().collect::<Vec<_>>(),
                &tick_spacings.iter().rev().copied().collect::<Vec<_>>()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactInputRequest {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub executor: Address,
    pub at: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactOutputRequest {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_out: U256,
    pub executor: Address,
    pub at: BlockRef,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuality {
    pub candidates_examined: usize,
    pub quote_calls: usize,
    pub budget_exhausted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteQuote {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    pub minimum_amount_out: U256,
    pub executor: Address,
    pub router: Option<Address>,
    pub route: RouteKind,
    pub validity: QuoteValidity,
    /// Diagnostic quoter estimate, not the liquidation gas limit.
    pub quoter_gas_estimate: U256,
    pub search: SearchQuality,
    pub fingerprint: B256,
}

impl RouteQuote {
    pub fn validate(&self, token_in: Address, token_out: Address, amount_in: U256, executor: Address, now: &BlockRef) -> RouteResult<()> {
        self.validity.validate(now)?;
        if self.token_in != token_in || self.token_out != token_out || self.amount_in != amount_in
            || self.executor != executor || self.amount_in.is_zero() || self.amount_out.is_zero()
            || self.minimum_amount_out > self.amount_out || self.fingerprint != self.compute_fingerprint()
        {
            return Err(RouteError::Identity("quote request/fingerprint differs".into()));
        }
        self.validate_kind()
    }

    fn validate_kind(&self) -> RouteResult<()> {
        match &self.route {
            RouteKind::Kyber { path } => kyber::validate_path(path, &ExactInputRequest { token_in: self.token_in, token_out: self.token_out,
                amount_in: self.amount_in, executor: self.executor, at: self.validity.as_of }, self.router.ok_or_else(|| RouteError::Identity("Kyber router missing".into()))?, self.minimum_amount_out),
            RouteKind::SameToken if self.token_in == self.token_out && self.router.is_none()
                && self.amount_in == self.amount_out => Ok(()),
            RouteKind::V3 { path, tokens, fees, pools } if self.router.is_some_and(|router| !router.is_zero())
                && tokens.first() == Some(&self.token_in) && tokens.last() == Some(&self.token_out)
                && pools.len() == fees.len() && pools.iter().all(|pool| !pool.is_zero())
                && encode_exact_input_path(tokens, fees)? == *path => Ok(()),
            RouteKind::V4 { path, tokens, fees, tick_spacings, pool_ids }
                if self.router.is_some_and(|router| !router.is_zero())
                && tokens.first() == Some(&self.token_in) && tokens.last() == Some(&self.token_out)
                && encode_v4_path(tokens, fees, tick_spacings)? == *path
                && uniswap_v4::pool_ids(tokens, fees, tick_spacings)? == *pool_ids => Ok(()),
            _ => Err(RouteError::Identity("quote route shape differs".into())),
        }
    }

    pub fn compute_fingerprint(&self) -> B256 {
        let mut bytes = Vec::from(b"barterlend-route-exact-input-v1".as_slice());
        encode_quote_common(&mut bytes, self.token_in, self.token_out, self.executor, self.router,
            self.amount_in, self.amount_out, self.minimum_amount_out, &self.route, &self.validity);
        keccak256(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactOutputQuote {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_out: U256,
    pub amount_in: U256,
    /// Round the slippage buffer up to cover the required MON amount.
    pub maximum_amount_in: U256,
    pub executor: Address,
    pub router: Option<Address>,
    pub route: RouteKind,
    pub reverse_path: Bytes,
    pub validity: QuoteValidity,
    pub quoter_gas_estimate: U256,
    pub search: SearchQuality,
    pub fingerprint: B256,
}

impl ExactOutputQuote {
    pub fn validate(&self, token_in: Address, token_out: Address, amount_out: U256, executor: Address, now: &BlockRef) -> RouteResult<()> {
        self.validity.validate(now)?;
        if self.token_in != token_in || self.token_out != token_out || self.amount_out != amount_out
            || self.executor != executor || self.amount_out.is_zero() || self.amount_in.is_zero()
            || self.maximum_amount_in < self.amount_in || self.fingerprint != self.compute_fingerprint()
            || self.reverse_path != self.route.exact_output_path()?
        {
            return Err(RouteError::Identity("exact-output quote request/fingerprint differs".into()));
        }
        let input = RouteQuote {
            token_in: self.token_in, token_out: self.token_out, amount_in: self.amount_in,
            amount_out: self.amount_out, minimum_amount_out: self.amount_out, executor: self.executor,
            router: self.router, route: self.route.clone(), validity: self.validity.clone(),
            quoter_gas_estimate: self.quoter_gas_estimate, search: self.search.clone(), fingerprint: B256::ZERO,
        };
        input.validate_kind()
    }

    pub fn compute_fingerprint(&self) -> B256 {
        let mut bytes = Vec::from(b"barterlend-route-exact-output-v1".as_slice());
        encode_quote_common(&mut bytes, self.token_in, self.token_out, self.executor, self.router,
            self.amount_in, self.amount_out, self.maximum_amount_in, &self.route, &self.validity);
        bytes.extend_from_slice(&(self.reverse_path.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&self.reverse_path);
        keccak256(bytes)
    }
}

#[async_trait]
pub trait RouteProvider: Send + Sync {
    async fn quote_exact_input(&self, request: &ExactInputRequest) -> RouteResult<RouteQuote>;
    async fn quote_exact_output(&self, request: &ExactOutputRequest) -> RouteResult<ExactOutputQuote>;
}

pub fn encode_exact_input_path(tokens: &[Address], fees: &[u32]) -> RouteResult<Bytes> {
    if !(tokens.len() == 2 || tokens.len() == 3) || fees.len() + 1 != tokens.len()
        || tokens.iter().any(|token| token.is_zero()) || fees.iter().any(|fee| *fee >= 1_000_000)
        || tokens.iter().enumerate().any(|(index, token)| tokens[..index].contains(token))
    {
        return Err(RouteError::InvalidConfig("V3 path must have 1..=2 hops, distinct nonzero tokens, fees < 1000000".into()));
    }
    let mut bytes = Vec::with_capacity(20 + 23 * fees.len());
    bytes.extend_from_slice(tokens[0].as_slice());
    for (fee, token) in fees.iter().zip(&tokens[1..]) {
        bytes.extend_from_slice(&fee.to_be_bytes()[1..]);
        bytes.extend_from_slice(token.as_slice());
    }
    Ok(bytes.into())
}

pub fn encode_exact_output_path(tokens: &[Address], fees: &[u32]) -> RouteResult<Bytes> {
    let tokens = tokens.iter().rev().copied().collect::<Vec<_>>();
    let fees = fees.iter().rev().copied().collect::<Vec<_>>();
    encode_exact_input_path(&tokens, &fees)
}

fn encode_quote_common(bytes: &mut Vec<u8>, token_in: Address, token_out: Address, executor: Address,
    router: Option<Address>, amount_in: U256, amount_out: U256, guarded_amount: U256,
    route: &RouteKind, validity: &QuoteValidity)
{
    for address in [token_in, token_out, executor, router.unwrap_or(Address::ZERO)] { bytes.extend_from_slice(address.as_slice()); }
    for amount in [amount_in, amount_out, guarded_amount] { bytes.extend_from_slice(&amount.to_be_bytes::<32>()); }
    match route {
        RouteKind::SameToken => bytes.push(0),
        RouteKind::Kyber { path } => { bytes.push(3); bytes.extend_from_slice(&(path.len() as u64).to_be_bytes()); bytes.extend_from_slice(path); },
        RouteKind::V4 { path, tokens, fees, tick_spacings, pool_ids } => {
            bytes.push(2);
            bytes.extend_from_slice(&(path.len() as u64).to_be_bytes());
            bytes.extend_from_slice(path);
            bytes.extend_from_slice(&(tokens.len() as u64).to_be_bytes());
            for token in tokens { bytes.extend_from_slice(token.as_slice()); }
            bytes.extend_from_slice(&(fees.len() as u64).to_be_bytes());
            for fee in fees { bytes.extend_from_slice(&fee.to_be_bytes()); }
            bytes.extend_from_slice(&(tick_spacings.len() as u64).to_be_bytes());
            for spacing in tick_spacings { bytes.extend_from_slice(&spacing.to_be_bytes()); }
            bytes.extend_from_slice(&(pool_ids.len() as u64).to_be_bytes());
            for id in pool_ids { bytes.extend_from_slice(id.as_slice()); }
        }
        RouteKind::V3 { path, tokens, fees, pools } => {
            bytes.push(1);
            bytes.extend_from_slice(&(path.len() as u64).to_be_bytes());
            bytes.extend_from_slice(path);
            for addresses in [tokens, pools] {
                bytes.extend_from_slice(&(addresses.len() as u64).to_be_bytes());
                for address in addresses { bytes.extend_from_slice(address.as_slice()); }
            }
            bytes.extend_from_slice(&(fees.len() as u64).to_be_bytes());
            for fee in fees { bytes.extend_from_slice(&fee.to_be_bytes()); }
        }
    }
    bytes.extend_from_slice(&validity.as_of.number.to_be_bytes());
    bytes.extend_from_slice(validity.as_of.hash.as_slice());
    bytes.extend_from_slice(validity.as_of.parent_hash.as_slice());
    bytes.extend_from_slice(&validity.as_of.timestamp.to_be_bytes());
    bytes.push(match validity.as_of.commitment { barterlend_state::Commitment::Finalized => 0, barterlend_state::Commitment::Safe => 1, barterlend_state::Commitment::Latest => 2 });
    bytes.extend_from_slice(&validity.expires_at.to_be_bytes());
    bytes.extend_from_slice(&validity.max_age_blocks.to_be_bytes());
}
