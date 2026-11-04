# Liquidity

`config init --network monad` fills all three providers from the bundled, code-hash-checked Mainnet preset and the executor's existing permissions. No API availability probe or approval transaction is performed. V4 coverage is the approved subset of three preset pools, not an unrestricted pool search.

V3 searches approved direct/two-hop paths and fee tiers. V4 uses configured pool keys and verified execution contracts. Kyber excludes RFQ sources (`excludeRFQSources=true`); its routes may use AMMs and on-chain orderbooks. RFQ reservations are unsupported. It uses GET `/monad/api/v1/routes`, then POST `/monad/api/v1/route/build`, with a separate **3 requests/s** API budget including retries.

Kyber targets Monad mainnet, not testnet. Optional `[kyber]` fields: `base_url`, `router`, `router_code_hash`, `client_id`, `api_key_env`, `requests_per_second`, `ttl_seconds`. Use [Kyber's contract registry](https://docs.kyberswap.com/developer-guide/aggregator-api/contracts), pin deployed bytecode, and configure the same identity through the owner's `setKyberRouter`. Bases: `https://aggregator-api.kyberswap.com` or `https://api.kyberswap.com/swap` with gateway authentication. See the [API specification](https://docs.kyberswap.com/developer-guide/aggregator-api/aggregator-api-specification/evm-swaps).

API calldata is untrusted. Rust and Solidity validate tokens, amount, recipient, target, selector, minimum output, fees and flags. ERC20 swaps use zero native value, exact temporary allowance and normal `call`. Permit data, partial-fill flags and API fee transfers are rejected. Existing balances cannot subsidize liquidation. Unused collateral/intermediate tokens stay withdrawable; their value is not assumed as profit.

Each comparison uses one freshly verified funding amount and one gas-valuation path. The bot samples up to eight routes, reserving a slot for every available provider/execution family (including V3, V4 and Kyber). Quotes and full gas-aware simulations run independently, with a 25-second limit per candidate. It compares conservative debt-token proceeds after repayment and the signed gas cost; a zero net outcome remains eligible with the default zero extra profit. Higher raw swap output alone does not win.

A failed or slow source does not discard other valid offers. The winning plan must still be within its block/time bounds after comparison. Candidates use the shared RPC budget. Configured candidate/quote limits can reduce the sample; a budget too small to include all available sources rejects the comparison. Failed sizes and gas-valuation paths rotate across visits (at most two combinations and 60 seconds per visit), and route samples rotate between passes. This is a bounded search, not a global optimizer across every route and size. Events `route_candidate_validated`, `route_candidate_failed` and `route_selected` expose outcomes without calldata or provider credentials.

## Add a provider

Implement `LiquidityProvider` (`src/routes/registry.rs`), then register it in app and doctor. Scheduler, math and signer stay unchanged. Example wrapping an existing execution-compatible source:

```rust,ignore
use std::sync::Arc;
use async_trait::async_trait;
use alloy::primitives::Address;
use barterlend_liquidator::routes::{ExactInputRequest, RouteKind, RouteQuote,
    RouteResult, SwapRouteProvider, registry::{Capabilities, LiquidityProvider}};
struct MyLiquidity(Arc<SwapRouteProvider>);
#[async_trait]
impl LiquidityProvider for MyLiquidity {
    fn name(&self) -> &'static str { "my-liquidity" }
    fn capabilities(&self) -> Capabilities {
        Capabilities { exact_input: true, exact_output: true }
    }
    fn candidates(&self, input: Address, output: Address) -> RouteResult<Vec<RouteKind>> {
        self.0.configured_routes(input, output)
    }
    async fn quote(&self, request: &ExactInputRequest, hint: &RouteKind) -> RouteResult<RouteQuote> {
        self.0.quote_exact_input_route(request, hint).await
    }
}
```

For another API, add bounded timeout, response size, retries, rate limiter and redacted errors. Validate every quote against its request. Keep `candidates` cheap and bounded; unsupported pairs return an empty list. A new swap ABI needs a reviewed Solidity execution path and a new deployment, not unchecked arbitrary calldata.

Required tests: wrong token/recipient/amount/target, old-balance spending, allowance cleanup, malicious callbacks, stale quotes, false outputs, atomic rollback and fallback. Finish with actual full liquidation on a pinned fork. Quote success alone is insufficient. Fee-on-transfer/rebasing/unusual callback tokens are not implicitly supported. Check both collateral→debt liquidity and MON→debt valuation for each real pair and size.
