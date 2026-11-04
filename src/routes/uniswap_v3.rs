//! V3 factory, QuoterV2 and Router02 integration without signing or arbitrary calldata.

use std::{collections::{BTreeMap, BTreeSet}, sync::Arc};

use alloy::{primitives::{Address, B256, Bytes, U256, U512, aliases::{I24, U24}, b256, keccak256}, sol, sol_types::{SolCall, SolValue}};
use async_trait::async_trait;
use barterlend_state::{BlockRef, ChainIdentity, HttpReader, PinnedRead, StateReader};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::*;

sol! {
    interface V3FactoryRead {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
        function feeAmountTickSpacing(uint24 fee) external view returns (int24);
    }
    interface V3PoolRead {
        function factory() external view returns (address);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
    }
    interface Router02Read {
        function factory() external view returns (address);
        function WETH9() external view returns (address);
    }
    interface QuoterV2Read {
        function factory() external view returns (address);
        function WETH9() external view returns (address);
        function quoteExactInput(bytes path, uint256 amountIn) external returns (
            uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate
        );
        function quoteExactOutput(bytes path, uint256 amountOut) external returns (
            uint256 amountIn, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate
        );
    }
    interface WrappedMetadataRead {
        function decimals() external view returns (uint8);
    }
}

pub const POOL_INIT_CODE_HASH: B256 = b256!("e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V3CodeHashes {
    pub factory: B256,
    pub router: B256,
    pub quoter: B256,
    pub wrapped_native: B256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V3Config {
    pub chain: ChainIdentity,
    pub factory: Address,
    pub router: Address,
    pub quoter: Address,
    pub wrapped_native: Address,
    pub code_hashes: V3CodeHashes,
    pub allowed_tokens: BTreeSet<Address>,
    pub intermediates: Vec<Address>,
    pub fee_tiers: Vec<u32>,
    /// Optional runtime pins. CREATE2, factory, token and fee checks are always required.
    pub pool_code_hashes: BTreeMap<Address, B256>,
    pub limits: RouteLimits,
}

impl V3Config {
    pub fn validate(&self) -> RouteResult<()> {
        self.limits.validate()?;
        let contracts = [self.factory, self.router, self.quoter, self.wrapped_native];
        if contracts.iter().any(|address| address.is_zero())
            || contracts.iter().enumerate().any(|(index, address)| contracts[..index].contains(address))
            || [self.code_hashes.factory, self.code_hashes.router, self.code_hashes.quoter, self.code_hashes.wrapped_native].iter().any(|hash| hash.is_zero())
            || self.chain.chain_id == 0 || self.chain.genesis_hash.is_zero()
            || self.allowed_tokens.is_empty() || self.allowed_tokens.iter().any(|token| token.is_zero())
            || !self.allowed_tokens.contains(&self.wrapped_native)
            || self.fee_tiers.is_empty() || self.fee_tiers.len() > 16 || self.intermediates.len() > 16
            || self.fee_tiers.iter().any(|fee| *fee >= 1_000_000)
            || self.fee_tiers.iter().collect::<BTreeSet<_>>().len() != self.fee_tiers.len()
            || self.intermediates.iter().collect::<BTreeSet<_>>().len() != self.intermediates.len()
            || self.intermediates.iter().any(|token| !self.allowed_tokens.contains(token))
            || self.pool_code_hashes.iter().any(|(address, hash)| address.is_zero() || hash.is_zero())
        {
            return Err(RouteError::InvalidConfig("V3 identities, token allowlist, distinct fee/intermediate lists are required".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V3Identity {
    pub chain: ChainIdentity,
    pub as_of: BlockRef,
    pub factory: Address,
    pub router: Address,
    pub quoter: Address,
    pub wrapped_native: Address,
    pub code_hashes: V3CodeHashes,
}

pub type V3RouteProvider = SwapRouteProvider;

/// Shared route catalog and bounded search over V3 plus optional explicit V4 pools.
pub struct SwapRouteProvider {
    v4: Option<super::uniswap_v4::V4RouteProvider>,
    cursors: std::sync::Mutex<BTreeMap<(Address, Address, bool), usize>>,
    reader: Arc<HttpReader>,
    config: V3Config,
    verified: Mutex<Option<VerifiedAt>>,
}

// Entries are valid only at one exact block. A code hash pin alone does not
// prove storage-backed getters immutable (for example on a proxy), so metadata
// is never reused across anchors. The map is bounded independently of token count.
const MAX_CACHED_HOPS: usize = 512;
struct VerifiedAt {
    identity: V3Identity,
    spacings: BTreeMap<u32, I24>,
    hops: BTreeMap<(Address, Address, u32), Option<Address>>,
}

#[derive(Clone)]
struct Candidate { tokens: Vec<Address>, fees: Vec<u32> }

impl SwapRouteProvider {
    pub fn new(reader: Arc<HttpReader>, config: V3Config) -> RouteResult<Self> {
        config.validate()?;
        Ok(Self { reader, config, verified: Mutex::new(None), v4: None, cursors: std::sync::Mutex::new(BTreeMap::new()) })
    }

    pub fn with_v4(mut self, config: Option<V4Config>) -> RouteResult<Self> {
        self.v4 = config.map(|config| super::uniswap_v4::V4RouteProvider::new(self.reader.clone(), config, self.config.clone())).transpose()?;
        Ok(self)
    }
    pub fn v4_quoter(&self) -> Option<Address> { self.v4.as_ref().map(|p| p.quoter()) }

    fn next_candidate(&self, a: Address, b: Address, output: bool, len: usize) -> RouteResult<usize> {
        let mut cursors = self.cursors.lock().map_err(|_| RouteError::Unavailable("route cursor lock poisoned".into()))?;
        let key = (a,b,output);
        if cursors.len() >= 512 && !cursors.contains_key(&key) { cursors.pop_first(); }
        let next = cursors.entry(key).or_insert(0);
        let selected = *next % len;
        *next = (selected + 1) % len; // Advance BEFORE any awaited RPC or cancellation.
        Ok(selected)
    }

    pub fn config(&self) -> &V3Config { &self.config }
    pub fn reader(&self) -> &Arc<HttpReader> { &self.reader }

    pub async fn verify_at(&self, at: &BlockRef) -> RouteResult<V3Identity> {
        if self.reader.identity().await? != self.config.chain {
            return Err(RouteError::Identity("RPC chain/genesis differ from route manifest".into()));
        }
        let mut cached = self.verified.lock().await;
        if let Some(verified) = cached.as_ref().filter(|verified| verified.identity.as_of == *at) {
            self.reader.ensure_canonical(at).await?;
            return Ok(verified.identity.clone());
        }
        let contracts = [
            (self.config.factory, self.config.code_hashes.factory, "factory"),
            (self.config.router, self.config.code_hashes.router, "router"),
            (self.config.quoter, self.config.code_hashes.quoter, "quoter"),
            (self.config.wrapped_native, self.config.code_hashes.wrapped_native, "wrapped native"),
        ];
        let mut reads = contracts.iter().map(|(address, _, _)| PinnedRead::Code { address: *address }).collect::<Vec<_>>();
        reads.extend([
            call_read(self.config.router, &Router02Read::factoryCall {}),
            call_read(self.config.router, &Router02Read::WETH9Call {}),
            call_read(self.config.quoter, &QuoterV2Read::factoryCall {}),
            call_read(self.config.quoter, &QuoterV2Read::WETH9Call {}),
            call_read(self.config.wrapped_native, &WrappedMetadataRead::decimalsCall {}),
        ]);
        for &fee in &self.config.fee_tiers {
            reads.push(call_read(self.config.factory, &V3FactoryRead::feeAmountTickSpacingCall { fee: U24::from(fee) }));
        }
        let values = self.reader.read_views(&reads, at).await?;
        for ((_, expected, role), code) in contracts.iter().zip(&values) {
            if code.is_empty() || keccak256(code) != *expected {
                return Err(RouteError::Identity(format!("{role} runtime code differs")));
            }
        }
        let router_factory = decode_return::<Router02Read::factoryCall>(&values[4])?;
        let router_wrapped = decode_return::<Router02Read::WETH9Call>(&values[5])?;
        let quoter_factory = decode_return::<QuoterV2Read::factoryCall>(&values[6])?;
        let quoter_wrapped = decode_return::<QuoterV2Read::WETH9Call>(&values[7])?;
        let decimals = decode_return::<WrappedMetadataRead::decimalsCall>(&values[8])?;
        if router_factory != self.config.factory || quoter_factory != self.config.factory
            || router_wrapped != self.config.wrapped_native || quoter_wrapped != self.config.wrapped_native || decimals != 18
        {
            return Err(RouteError::Identity("router/quoter factory/WETH9 or wrapped decimals differ".into()));
        }
        let mut spacings = BTreeMap::new();
        for (&fee, value) in self.config.fee_tiers.iter().zip(&values[9..]) {
            let spacing = decode_return::<V3FactoryRead::feeAmountTickSpacingCall>(value)?;
            if spacing <= I24::ZERO {
                return Err(RouteError::InvalidConfig(format!("fee tier {fee} is not enabled in factory")));
            }
            spacings.insert(fee, spacing);
        }
        // read_views pins these values; an enclosing verified scope rechecks
        // the anchor before its caller can use the prepared plan.
        let identity = V3Identity {
            chain: self.config.chain, as_of: *at, factory: self.config.factory, router: self.config.router,
            quoter: self.config.quoter, wrapped_native: self.config.wrapped_native, code_hashes: self.config.code_hashes.clone(),
        };
        *cached = Some(VerifiedAt { identity: identity.clone(), spacings, hops: BTreeMap::new() });
        Ok(identity)
    }

    fn validate_request(&self, token_in: Address, token_out: Address, amount: U256, executor: Address) -> RouteResult<()> {
        if amount.is_zero() || executor.is_zero() || !self.config.allowed_tokens.contains(&token_in)
            || !self.config.allowed_tokens.contains(&token_out)
        {
            return Err(RouteError::InvalidConfig("quote requires nonzero amount/executor and allowed tokens".into()));
        }
        Ok(())
    }

    fn candidates(&self, token_in: Address, token_out: Address) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        // The manifest bounds this catalog to 16 + 16*16*16 = 4112 paths.
        // Per-pass budgets must not permanently hide the tail of the catalog.
        let mut add = |candidate| { candidates.push(candidate); };
        for &fee in &self.config.fee_tiers { add(Candidate { tokens: vec![token_in, token_out], fees: vec![fee] }); }
        for &middle in &self.config.intermediates {
            if middle == token_in || middle == token_out { continue; }
            for &first in &self.config.fee_tiers {
                for &second in &self.config.fee_tiers {
                    add(Candidate { tokens: vec![token_in, middle, token_out], fees: vec![first, second] });
                }
            }
        }
        candidates
    }

    /// Enumerates the complete bounded catalog without RPC; budgets limit work per pass.
    /// The derived pool addresses do not prove that pools exist or are trusted.
    /// Execution must use `quote_exact_input_route` at a fresh anchor, which
    /// checks every identity and obtains an actual quote for the selected path.
    pub fn configured_routes(&self, token_in: Address, token_out: Address) -> RouteResult<Vec<RouteKind>> {
        if !self.config.allowed_tokens.contains(&token_in) || !self.config.allowed_tokens.contains(&token_out) {
            return Err(RouteError::InvalidConfig("configured path requires allowed tokens".into()));
        }
        if token_in == token_out { return Ok(vec![RouteKind::SameToken]); }
        let mut routes = if let Some(v4) = &self.v4 { v4.configured_routes(token_in, token_out)? } else { Vec::new() };
        let v3 = self.candidates(token_in, token_out).into_iter().map(|candidate| {
            let path = encode_exact_input_path(&candidate.tokens, &candidate.fees)?;
            let pools = candidate.tokens.windows(2).zip(&candidate.fees)
                .map(|(tokens, &fee)| compute_pool_address(self.config.factory, tokens[0], tokens[1], fee))
                .collect::<RouteResult<Vec<_>>>()?;
            Ok(RouteKind::V3 { path, tokens: candidate.tokens, fees: candidate.fees, pools })
        }).collect::<RouteResult<Vec<_>>>()?;
        routes.extend(v3);
        Ok(routes)
    }

    async fn resolve(&self, candidate: &Candidate, at: &BlockRef) -> RouteResult<Option<Vec<Address>>> {
        let mut pools = Vec::new();
        for (tokens, &fee) in candidate.tokens.windows(2).zip(&candidate.fees) {
            let (expected0, expected1) = if tokens[0] < tokens[1] { (tokens[0], tokens[1]) } else { (tokens[1], tokens[0]) };
            let key = (expected0, expected1, fee);
            let (cached_pool, cached_spacing) = {
                let cached = self.verified.lock().await;
                match cached.as_ref().filter(|verified| verified.identity.as_of == *at) {
                    Some(verified) => (verified.hops.get(&key).copied(), verified.spacings.get(&fee).copied()),
                    None => (None, None),
                }
            };
            if let Some(pool) = cached_pool {
                match pool { Some(pool) => { pools.push(pool); continue; }, None => return Ok(None) }
            }
            let call = V3FactoryRead::getPoolCall { tokenA: tokens[0], tokenB: tokens[1], fee: U24::from(fee) };
            let pool = self.reader.contract_call(self.config.factory, &call, at).await?;
            if pool.is_zero() { self.cache_hop(at, key, None).await; return Ok(None); }
            if pool != compute_pool_address(self.config.factory, tokens[0], tokens[1], fee)? {
                return Err(RouteError::Identity("getPool does not match canonical CREATE2 address".into()));
            }
            let values = self.reader.read_views(&[
                PinnedRead::Code { address: pool },
                call_read(pool, &V3PoolRead::factoryCall {}),
                call_read(pool, &V3PoolRead::token0Call {}),
                call_read(pool, &V3PoolRead::token1Call {}),
                call_read(pool, &V3PoolRead::feeCall {}),
                call_read(pool, &V3PoolRead::tickSpacingCall {}),
            ], at).await?;
            let code = &values[0];
            if code.is_empty() || self.config.pool_code_hashes.get(&pool).is_some_and(|hash| *hash != keccak256(code)) {
                return Err(RouteError::Identity("pool runtime code differs or is empty".into()));
            }
            let factory = decode_return::<V3PoolRead::factoryCall>(&values[1])?;
            let token0 = decode_return::<V3PoolRead::token0Call>(&values[2])?;
            let token1 = decode_return::<V3PoolRead::token1Call>(&values[3])?;
            let actual_fee = decode_return::<V3PoolRead::feeCall>(&values[4])?;
            let spacing = decode_return::<V3PoolRead::tickSpacingCall>(&values[5])?;
            let expected_spacing = match cached_spacing {
                Some(spacing) => spacing,
                None => self.reader.contract_call(self.config.factory,
                    &V3FactoryRead::feeAmountTickSpacingCall { fee: U24::from(fee) }, at).await?,
            };
            if factory != self.config.factory || token0 != expected0 || token1 != expected1
                || actual_fee != U24::from(fee) || spacing != expected_spacing || spacing <= I24::ZERO
            {
                return Err(RouteError::Identity("pool factory/token/fee/tickSpacing differs".into()));
            }
            self.cache_hop(at, key, Some(pool)).await;
            pools.push(pool);
        }
        Ok(Some(pools))
    }

    async fn cache_hop(&self, at: &BlockRef, key: (Address, Address, u32), pool: Option<Address>) {
        let mut cached = self.verified.lock().await;
        if let Some(verified) = cached.as_mut().filter(|verified| verified.identity.as_of == *at) {
            // Eviction costs extra reads, never weaker identity checks.
            if verified.hops.len() >= MAX_CACHED_HOPS { verified.hops.clear(); }
            verified.hops.insert(key, pool);
        }
    }

    fn selected_candidate(&self, route: &RouteKind, token_in: Address, token_out: Address) -> RouteResult<Option<Candidate>> {
        match route {
            RouteKind::SameToken if token_in == token_out => Ok(None),
            RouteKind::V3 { path, tokens, fees, pools } => {
                if tokens.first() != Some(&token_in) || tokens.last() != Some(&token_out)
                    || encode_exact_input_path(tokens, fees)? != *path || pools.len() != fees.len()
                    || tokens.iter().any(|token| !self.config.allowed_tokens.contains(token))
                    || fees.iter().any(|fee| !self.config.fee_tiers.contains(fee))
                    || (tokens.len() == 3 && !self.config.intermediates.contains(&tokens[1]))
                {
                    return Err(RouteError::Identity("selected route is outside configured paths".into()));
                }
                for ((tokens, &fee), pool) in tokens.windows(2).zip(fees).zip(pools) {
                    if *pool != compute_pool_address(self.config.factory, tokens[0], tokens[1], fee)? {
                        return Err(RouteError::Identity("selected pool differs from canonical CREATE2 address".into()));
                    }
                }
                Ok(Some(Candidate { tokens: tokens.clone(), fees: fees.clone() }))
            }
            _ => Err(RouteError::Identity("selected route token pair differs".into())),
        }
    }

    async fn same_input(&self, request: &ExactInputRequest) -> RouteResult<RouteQuote> {
        let validity = QuoteValidity::at(request.at, self.config.limits.quote_ttl_seconds, self.config.limits.max_age_blocks)?;
        if self.reader.identity().await? != self.config.chain {
            return Err(RouteError::Identity("RPC chain/genesis differ from route manifest".into()));
        }
        self.reader.ensure_canonical(&request.at).await?;
        let mut quote = RouteQuote {
            token_in: request.token_in, token_out: request.token_out, amount_in: request.amount_in,
            amount_out: request.amount_in, minimum_amount_out: request.amount_in, executor: request.executor,
            router: None, route: RouteKind::SameToken, validity, quoter_gas_estimate: U256::ZERO,
            search: SearchQuality { candidates_examined: 0, quote_calls: 0, budget_exhausted: false }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(quote)
    }

    async fn same_output(&self, request: &ExactOutputRequest) -> RouteResult<ExactOutputQuote> {
        self.verify_at(&request.at).await?;
        let validity = QuoteValidity::at(request.at, self.config.limits.quote_ttl_seconds, self.config.limits.max_age_blocks)?;
        let mut quote = ExactOutputQuote {
            token_in: request.token_in, token_out: request.token_out, amount_out: request.amount_out,
            amount_in: request.amount_out, maximum_amount_in: request.amount_out, executor: request.executor,
            router: None, route: RouteKind::SameToken, reverse_path: Bytes::new(), validity,
            quoter_gas_estimate: U256::ZERO,
            search: SearchQuality { candidates_examined: 0, quote_calls: 0, budget_exhausted: false }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(quote)
    }

    /// Re-quotes one previously selected path at the request's exact anchor.
    /// Only the path is reused: pool identities are checked at this block and
    /// Quoter is called again for this amount. Old prices/expiries are never reused.
    pub async fn quote_exact_input_route(&self, request: &ExactInputRequest, route: &RouteKind) -> RouteResult<RouteQuote> {
        self.probe_input(request, route).await?.0.ok_or_else(|| RouteError::Unavailable("selected input route cannot quote this amount".into()))
    }

    async fn probe_input(&self, request: &ExactInputRequest, route: &RouteKind) -> RouteResult<(Option<RouteQuote>, usize)> {
        if matches!(route, RouteKind::V4 { .. }) {
            return match self.v4.as_ref().ok_or_else(|| RouteError::InvalidConfig("V4 is not configured".into()))?.quote_input(request, route).await {
                Ok(quote) => Ok((Some(quote), 1)),
                Err(RouteError::Unavailable(_)) => Ok((None, 1)),
                Err(error) => Err(error),
            };
        }
        self.validate_request(request.token_in, request.token_out, request.amount_in, request.executor)?;
        let Some(candidate) = self.selected_candidate(route, request.token_in, request.token_out)? else {
            return Ok((Some(self.same_input(request).await?), 0));
        };
        self.verify_at(&request.at).await?;
        let Some(pools) = self.resolve(&candidate, &request.at).await? else { return Ok((None, 0)); };
        let validity = QuoteValidity::at(request.at, self.config.limits.quote_ttl_seconds, self.config.limits.max_age_blocks)?;
        let quote = self.quote_input_candidate(request, &candidate, pools, validity).await?;
        // The last Quoter contract_call brackets its read with canonical headers.
        Ok((quote, 1))
    }

    /// Exact-output counterpart for the selected native-gas conversion path.
    /// A different amount (for example a new gas bound) always gets a new quote.
    pub async fn quote_exact_output_route(&self, request: &ExactOutputRequest, route: &RouteKind) -> RouteResult<ExactOutputQuote> {
        self.probe_output(request, route).await?.0.ok_or_else(|| RouteError::Unavailable("selected output route cannot quote this amount".into()))
    }

    async fn probe_output(&self, request: &ExactOutputRequest, route: &RouteKind) -> RouteResult<(Option<ExactOutputQuote>, usize)> {
        if matches!(route, RouteKind::V4 { .. }) {
            return match self.v4.as_ref().ok_or_else(|| RouteError::InvalidConfig("V4 is not configured".into()))?.quote_output(request, route).await {
                Ok(quote) => Ok((Some(quote), 1)),
                Err(RouteError::Unavailable(_)) => Ok((None, 1)),
                Err(error) => Err(error),
            };
        }
        self.validate_request(request.token_in, request.token_out, request.amount_out, request.executor)?;
        let Some(candidate) = self.selected_candidate(route, request.token_in, request.token_out)? else {
            return Ok((Some(self.same_output(request).await?), 0));
        };
        self.verify_at(&request.at).await?;
        let Some(pools) = self.resolve(&candidate, &request.at).await? else { return Ok((None, 0)); };
        let validity = QuoteValidity::at(request.at, self.config.limits.quote_ttl_seconds, self.config.limits.max_age_blocks)?;
        let quote = self.quote_output_candidate(request, &candidate, pools, validity).await?;
        // The last Quoter contract_call brackets its read with canonical headers.
        Ok((quote, 1))
    }

    async fn quote_input_candidate(&self, request: &ExactInputRequest, candidate: &Candidate,
        pools: Vec<Address>, validity: QuoteValidity) -> RouteResult<Option<RouteQuote>>
    {
        let path = encode_exact_input_path(&candidate.tokens, &candidate.fees)?;
        let call = QuoterV2Read::quoteExactInputCall { path: path.clone(), amountIn: request.amount_in };
        let result = match self.reader.contract_call(self.config.quoter, &call, &request.at).await {
            Ok(result) => result,
            Err(error) if quote_revert(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if result.amountOut.is_zero() { return Ok(None); }
        if result.sqrtPriceX96AfterList.len() != candidate.fees.len() || result.initializedTicksCrossedList.len() != candidate.fees.len() {
            return Err(RouteError::Identity("Quoter result hop arrays differ from path".into()));
        }
        let minimum = mul_div(result.amountOut, U256::from(10_000 - self.config.limits.slippage_bps), U256::from(10_000), false)?;
        let mut quote = RouteQuote {
            token_in: request.token_in, token_out: request.token_out, amount_in: request.amount_in,
            amount_out: result.amountOut, minimum_amount_out: minimum, executor: request.executor,
            router: Some(self.config.router), route: RouteKind::V3 { path, tokens: candidate.tokens.clone(), fees: candidate.fees.clone(), pools },
            validity, quoter_gas_estimate: result.gasEstimate,
            search: SearchQuality { candidates_examined: 1, quote_calls: 1, budget_exhausted: false }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(Some(quote))
    }

    async fn quote_output_candidate(&self, request: &ExactOutputRequest, candidate: &Candidate,
        pools: Vec<Address>, validity: QuoteValidity) -> RouteResult<Option<ExactOutputQuote>>
    {
        let path = encode_exact_input_path(&candidate.tokens, &candidate.fees)?;
        let reverse = encode_exact_output_path(&candidate.tokens, &candidate.fees)?;
        let call = QuoterV2Read::quoteExactOutputCall { path: reverse.clone(), amountOut: request.amount_out };
        let result = match self.reader.contract_call(self.config.quoter, &call, &request.at).await {
            Ok(result) => result,
            Err(error) if quote_revert(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if result.amountIn.is_zero() { return Ok(None); }
        if result.sqrtPriceX96AfterList.len() != candidate.fees.len() || result.initializedTicksCrossedList.len() != candidate.fees.len() {
            return Err(RouteError::Identity("Quoter exact-output hop arrays differ".into()));
        }
        let maximum = mul_div(result.amountIn, U256::from(10_000 + self.config.limits.slippage_bps), U256::from(10_000), true)?;
        let mut quote = ExactOutputQuote {
            token_in: request.token_in, token_out: request.token_out, amount_out: request.amount_out,
            amount_in: result.amountIn, maximum_amount_in: maximum, executor: request.executor,
            router: Some(self.config.router), route: RouteKind::V3 { path, tokens: candidate.tokens.clone(), fees: candidate.fees.clone(), pools },
            reverse_path: reverse, validity, quoter_gas_estimate: result.gasEstimate,
            search: SearchQuality { candidates_examined: 1, quote_calls: 1, budget_exhausted: false }, fingerprint: B256::ZERO,
        };
        quote.fingerprint = quote.compute_fingerprint();
        Ok(Some(quote))
    }

    /// Successful quotes from a bounded search, ordered by output. The planner can simulate
    /// a cheaper route when the first route does not cover gas.
    pub async fn quote_exact_input_candidates(&self, request: &ExactInputRequest) -> RouteResult<Vec<RouteQuote>> {
        self.validate_request(request.token_in, request.token_out, request.amount_in, request.executor)?;
        if request.token_in == request.token_out { return Ok(vec![self.same_input(request).await?]); }
        let candidates = self.configured_routes(request.token_in, request.token_out)?;
        let budget = self.config.limits.max_candidates.min(self.config.limits.max_quote_calls).min(candidates.len());
        let mut quotes = Vec::new();
        let mut calls = 0;
        for _ in 0..budget {
            let index = self.next_candidate(request.token_in, request.token_out, false, candidates.len())?;
            let (quote, used) = self.probe_input(request, &candidates[index]).await?;
            calls += used;
            if let Some(quote) = quote { quotes.push(quote); }
        }
        let examined = budget;
        let truncated = budget < candidates.len();
        self.reader.ensure_canonical(&request.at).await?;
        for quote in &mut quotes { quote.search = SearchQuality { candidates_examined: examined, quote_calls: calls, budget_exhausted: truncated }; }
        quotes.sort_by(|a, b| b.amount_out.cmp(&a.amount_out).then_with(|| a.route.exact_input_path().cmp(&b.route.exact_input_path())));
        if quotes.is_empty() { return Err(RouteError::Unavailable("no executable exact-input route within search budget".into())); }
        Ok(quotes)
    }
}

#[async_trait]
impl RouteProvider for SwapRouteProvider {
    async fn quote_exact_input(&self, request: &ExactInputRequest) -> RouteResult<RouteQuote> {
        let quotes = self.quote_exact_input_candidates(request).await?;
        quotes.into_iter().next().ok_or_else(|| RouteError::Unavailable("no exact-input route".into()))
    }

    async fn quote_exact_output(&self, request: &ExactOutputRequest) -> RouteResult<ExactOutputQuote> {
        self.validate_request(request.token_in, request.token_out, request.amount_out, request.executor)?;
        // Native cost must verify the wrapped token identity even for a 1:1 conversion.
        self.verify_at(&request.at).await?;
        if request.token_in == request.token_out { return self.same_output(request).await; }
        let candidates = self.configured_routes(request.token_in, request.token_out)?;
        let budget = self.config.limits.max_candidates.min(self.config.limits.max_quote_calls).min(candidates.len());
        let mut best: Option<ExactOutputQuote> = None;
        let mut calls = 0;
        for _ in 0..budget {
            let index = self.next_candidate(request.token_in, request.token_out, true, candidates.len())?;
            let (quote, used) = self.probe_output(request, &candidates[index]).await?;
            calls += used;
            if let Some(quote) = quote {
                if best.as_ref().is_none_or(|current| quote.amount_in < current.amount_in) { best = Some(quote); }
            }
        }
        let examined = budget;
        let truncated = budget < candidates.len();
        self.reader.ensure_canonical(&request.at).await?;
        let mut best = best.ok_or_else(|| RouteError::Unavailable("no exact-output route within search budget".into()))?;
        best.search = SearchQuality { candidates_examined: examined, quote_calls: calls, budget_exhausted: truncated };
        Ok(best)
    }
}

pub fn compute_pool_address(factory: Address, token_a: Address, token_b: Address, fee: u32) -> RouteResult<Address> {
    if factory.is_zero() || token_a.is_zero() || token_b.is_zero() || token_a == token_b || fee >= 1_000_000 {
        return Err(RouteError::InvalidConfig("invalid canonical V3 pool key".into()));
    }
    let (token0, token1) = if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
    let salt = keccak256((token0, token1, U24::from(fee)).abi_encode());
    let mut bytes = vec![0xff];
    bytes.extend_from_slice(factory.as_slice());
    bytes.extend_from_slice(salt.as_slice());
    bytes.extend_from_slice(POOL_INIT_CODE_HASH.as_slice());
    let hash = keccak256(bytes);
    Ok(Address::from_slice(&hash.as_slice()[12..]))
}

fn call_read<C: SolCall>(to: Address, call: &C) -> PinnedRead {
    PinnedRead::Call { to, data: call.abi_encode().into() }
}

fn decode_return<C: SolCall>(bytes: &[u8]) -> RouteResult<C::Return> {
    crate::abi_validation::decode_return::<C>(bytes).map_err(Into::into)
}

pub(super) fn quote_revert(error: &barterlend_state::Error) -> bool {
    match error {
        barterlend_state::Error::Rpc { code, message } => *code == 3 || message.to_ascii_lowercase().contains("execution reverted"),
        _ => false,
    }
}

pub(super) fn mul_div(value: U256, numerator: U256, denominator: U256, ceil: bool) -> RouteResult<U256> {
    if denominator.is_zero() { return Err(RouteError::Arithmetic("zero divisor".into())); }
    let product = U512::from(value) * U512::from(numerator);
    let divisor = U512::from(denominator);
    let mut quotient = product / divisor;
    if ceil && product % divisor != U512::ZERO {
        quotient = quotient.checked_add(U512::from(1)).ok_or_else(|| RouteError::Arithmetic("ceil overflow".into()))?;
    }
    let limbs = quotient.as_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return Err(RouteError::Arithmetic("ratio result exceeds U256".into()));
    }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}
