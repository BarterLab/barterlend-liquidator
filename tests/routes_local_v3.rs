//! Explicit opt-in: genuine local V3 artifacts, read-only calls, no signer.

use std::{collections::{BTreeMap, BTreeSet}, sync::Arc, time::Duration};

use alloy::primitives::{Address, B256, U256, keccak256};
use barterlend_liquidator::{
    economics::{EconomicsError, NativeCostOracle, NativeCostQuality, NativeRatioFeedConfig,
        RatioNativeCostOracle, V3NativeCostOracle, WmonNativeCostOracle, WrappedNativeConfig},
    routes::{ExactInputRequest, ExactOutputRequest, RouteError, RouteKind, RouteLimits,
        RouteProvider, V3CodeHashes, V3Config, V3RouteProvider},
};
use barterlend_state::{Commitment, HttpReader, PoolSpec, StateReader, rpc::RpcConfig};
use serde::Deserialize;

#[derive(Deserialize)]
struct LocalManifest {
    chain_id: u64,
    genesis_hash: B256,
    deployment_block: u64,
    pool: Address,
    asset: Address,
    collateral: Address,
    oracle: Address,
    factory: Address,
    router: Address,
    quoter: Address,
    wrapped_native: Address,
    executor: Address,
    runtime_code_hashes: BTreeMap<String,B256>,
}

#[tokio::test]
#[ignore = "requires explicitly provided loopback seeded Anvil and local fixture manifest"]
async fn genuine_local_v3_routes_and_native_cost_sources() {
    let url = std::env::var("BARTERLEND_LOCAL_RPC_URL")
        .expect("BARTERLEND_LOCAL_RPC_URL is required; absence is not a passing integration test");
    barterlend_liquidator::tx::rpc::validate_loopback_url(&url).expect("strict local RPC URL");
    let path = std::env::var("BARTERLEND_LOCAL_MANIFEST").expect("BARTERLEND_LOCAL_MANIFEST is required");
    let manifest: LocalManifest = serde_json::from_slice(&std::fs::read(path).expect("local manifest file"))
        .expect("local manifest schema");
    assert_eq!(manifest.chain_id, 31337, "fixture is deliberately not a production chain");
    let reader = Arc::new(HttpReader::new(&url, RpcConfig {
        min_request_interval: Duration::ZERO, max_retries: 0, ..RpcConfig::default()
    }).expect("read-only reader"));
    let chain = reader.identity().await.expect("local chain identity");
    assert_eq!(chain.chain_id, manifest.chain_id);
    assert_eq!(chain.genesis_hash, manifest.genesis_hash, "isolated node genesis must match fixture manifest");
    let at = reader.head(Commitment::Latest).await.expect("chosen canonical local block");
    let hashes=V3CodeHashes { factory:manifest.runtime_code_hashes["factory"],router:manifest.runtime_code_hashes["router"],quoter:manifest.runtime_code_hashes["quoter"],wrapped_native:manifest.runtime_code_hashes["wrapped_native"] };
    let config = V3Config {
        chain, factory: manifest.factory, router: manifest.router, quoter: manifest.quoter,
        wrapped_native: manifest.wrapped_native, code_hashes: hashes.clone(),
        allowed_tokens: BTreeSet::from([manifest.asset, manifest.collateral, manifest.wrapped_native]),
        intermediates: vec![manifest.wrapped_native], fee_tiers: vec![500, 3000],
        pool_code_hashes: BTreeMap::new(),
        limits: RouteLimits { quote_ttl_seconds: 300, max_age_blocks: 1000, ..RouteLimits::default() },
    };
    let provider = Arc::new(V3RouteProvider::new(reader.clone(), config.clone()).expect("explicit local V3 config"));
    let identity = provider.verify_at(&at).await.expect("true factory/router/quoter/WMON identities");
    assert_eq!(identity.as_of, at);
    assert_eq!(identity.code_hashes, hashes);

    let amount = U256::from(1000);
    let request = ExactInputRequest { token_in: manifest.collateral, token_out: manifest.asset,
        amount_in: amount, executor: manifest.executor, at };
    let quotes = provider.quote_exact_input_candidates(&request).await.expect("genuine pool quotes");
    assert!(quotes.windows(2).all(|pair| pair[0].amount_out >= pair[1].amount_out));
    for quote in &quotes {
        quote.validate(request.token_in, request.token_out, amount, request.executor, &at).expect("input fingerprint and bounds");
        assert_eq!(quote.router, Some(manifest.router));
        assert!(quote.amount_out > U256::ZERO && quote.minimum_amount_out <= quote.amount_out);
    }
    for expected_fees in [vec![500], vec![3000], vec![3000, 3000]] {
        assert!(quotes.iter().any(|quote| matches!(&quote.route,
            RouteKind::V3 { fees, .. } if *fees == expected_fees)), "missing direct or two-hop fixture route");
    }
    let selected_input = provider.quote_exact_input_route(&request, &quotes[0].route).await.expect("selected genuine path is re-quoted");
    assert_eq!(selected_input.amount_out, quotes[0].amount_out);
    assert_eq!(selected_input.fingerprint, quotes[0].fingerprint);
    assert_eq!(selected_input.search.quote_calls, 1);
    let reverse = provider.quote_exact_input(&ExactInputRequest {
        token_in: manifest.asset, token_out: manifest.collateral, amount_in: amount,
        executor: manifest.executor, at,
    }).await.expect("opposite token orientation");
    reverse.validate(manifest.asset, manifest.collateral, amount, manifest.executor, &at).unwrap();

    let native_bound = U256::from(1000);
    let output = provider.quote_exact_output(&ExactOutputRequest {
        token_in: manifest.asset, token_out: manifest.wrapped_native, amount_out: native_bound,
        executor: manifest.executor, at,
    }).await.expect("genuine reverse-path exactOutput");
    output.validate(manifest.asset, manifest.wrapped_native, native_bound, manifest.executor, &at).unwrap();
    assert!(output.amount_in > U256::ZERO && output.maximum_amount_in >= output.amount_in);
    assert_eq!(&output.reverse_path[..20], manifest.wrapped_native.as_slice());
    assert_eq!(&output.reverse_path[output.reverse_path.len() - 20..], manifest.asset.as_slice());
    assert_eq!(output.reverse_path, output.route.exact_output_path().unwrap());
    if let RouteKind::V3 { path, .. } = &output.route { assert_ne!(path, &output.reverse_path); }

    let selected_output = provider.quote_exact_output_route(&ExactOutputRequest {
        token_in: manifest.asset, token_out: manifest.wrapped_native, amount_out: native_bound,
        executor: manifest.executor, at,
    }, &output.route).await.expect("selected native conversion path is re-quoted");
    assert_eq!(selected_output.amount_in, output.amount_in);
    assert_eq!(selected_output.fingerprint, output.fingerprint);
    assert_eq!(selected_output.search.quote_calls, 1);

    let native_source = V3NativeCostOracle::new(provider.clone(), manifest.executor, 100).unwrap();
    let cost = native_source.quote_cost(manifest.asset, native_bound, &at).await.expect("replenishment native conversion");
    cost.validate(manifest.asset, native_bound, &at).unwrap();
    assert!(cost.cost_asset >= output.maximum_amount_in);
    assert!(matches!(cost.quality, NativeCostQuality::V3ExactOutput { quoter, .. } if quoter == manifest.quoter));
    let cached_cost = native_source.quote_cost(manifest.asset, native_bound, &at).await.unwrap();
    assert_eq!(cached_cost, cost, "identical gas estimation iterations reuse exact-anchor cost data");
    let changed_bound = native_bound + U256::from(1);
    let selected = native_source.replenishment_quote(manifest.asset, changed_bound, &at).await.unwrap();
    selected.validate(manifest.asset, manifest.wrapped_native, changed_bound, manifest.executor, &at).unwrap();
    assert_eq!(selected.search.candidates_examined, 1, "gas iterations reuse the selected path instead of repeating full search");
    assert_eq!(selected.search.quote_calls, 1, "a different gas bound still requires a fresh exact-output quote");
    let changed_cost = native_source.quote_cost(manifest.asset, changed_bound, &at).await.unwrap();
    assert_eq!(changed_cost.native_bound, changed_bound);
    assert!(changed_cost.cost_asset >= selected.maximum_amount_in);
    assert!(native_source.select_route(manifest.asset, &RouteKind::SameToken).await.is_err(),
        "explicit native path must belong to the configured token pair");
    native_source.select_route(manifest.asset, &output.route).await.unwrap();
    let explicit_cost = native_source.quote_cost(manifest.asset, native_bound, &at).await.unwrap();
    explicit_cost.validate(manifest.asset, native_bound, &at).unwrap();
    let mut missing_native_path = None;
    for route in provider.configured_routes(manifest.asset, manifest.wrapped_native).unwrap() {
        if matches!(provider.quote_exact_output_route(&ExactOutputRequest {
            token_in:manifest.asset,token_out:manifest.wrapped_native,amount_out:native_bound,
            executor:manifest.executor,at,
        }, &route).await, Err(RouteError::Unavailable(_))) { missing_native_path=Some(route);break; }
    }
    let missing_native_path=missing_native_path.expect("fixture must include an absent configured native-conversion pool");
    native_source.select_route(manifest.asset, &missing_native_path).await.unwrap();
    assert!(matches!(native_source.quote_cost(manifest.asset, native_bound, &at).await,
        Err(EconomicsError::Route(RouteError::Unavailable(_)))),
        "an explicit unavailable native path must not reuse another path's cached price or start a hidden full search");
    native_source.select_route(manifest.asset, &output.route).await.unwrap();
    native_source.quote_cost(manifest.asset, native_bound, &at).await.unwrap();
    let wrapped_cost = native_source.quote_cost(manifest.wrapped_native, native_bound, &at).await.unwrap();
    assert_eq!(wrapped_cost.cost_asset, native_bound, "WMON/MON identity has no price or decimal scaling");

    let wrapped_config = WrappedNativeConfig { chain, address: manifest.wrapped_native,
        code_hash: hashes.wrapped_native, quote_ttl_seconds: 300, max_age_blocks: 1000 };
    let identity_source = WmonNativeCostOracle::new(reader.clone(), wrapped_config.clone()).unwrap();
    assert_eq!(identity_source.quote_cost(manifest.wrapped_native, native_bound, &at).await.unwrap().cost_asset, native_bound);
    assert!(matches!(identity_source.quote_cost(manifest.asset, native_bound, &at).await,
        Err(EconomicsError::CostUnavailable(_))));

    let same = provider.quote_exact_input(&ExactInputRequest { token_in: manifest.asset, token_out: manifest.asset,
        amount_in: amount, executor: manifest.executor, at }).await.unwrap();
    assert!(matches!(same.route, RouteKind::SameToken));
    assert_eq!((same.amount_out, same.minimum_amount_out, same.router), (amount, amount, None));

    for wrong_genesis in [false, true] {
        let mut wrong_identity = config.clone();
        if wrong_genesis { wrong_identity.chain.genesis_hash = B256::repeat_byte(0xfe); }
        else { wrong_identity.chain.chain_id += 1; }
        let wrong_provider = V3RouteProvider::new(reader.clone(), wrong_identity).unwrap();
        assert!(matches!(wrong_provider.quote_exact_input(&ExactInputRequest {
            token_in: manifest.asset, token_out: manifest.asset, amount_in: amount,
            executor: manifest.executor, at,
        }).await, Err(RouteError::Identity(_))), "same-token quotes must retain the explicit chain/genesis pin");
    }

    let mut wrong = config.clone();
    wrong.code_hashes.quoter = B256::repeat_byte(0xff);
    let wrong_provider = V3RouteProvider::new(reader.clone(), wrong).unwrap();
    assert!(matches!(wrong_provider.verify_at(&at).await, Err(RouteError::Identity(_))),
        "address with a wrong runtime pin must not quote");

    let mut bounded = config;
    bounded.limits.max_candidates = 1;
    bounded.limits.max_quote_calls = 1;
    let bounded_provider = V3RouteProvider::new(reader.clone(), bounded).unwrap();
    let bounded_quote = bounded_provider.quote_exact_input(&request).await.unwrap();
    assert_eq!(bounded_quote.search.quote_calls, 1);
    assert!(bounded_quote.search.budget_exhausted);

    let pool = reader.describe_pool(&PoolSpec { address: manifest.pool, deployment_block: manifest.deployment_block,
        expected_version: 5, deployment_block_hash: None }, &at).await.expect("real protocol pool descriptor");
    assert_eq!(pool.asset.address, manifest.asset);
    assert_eq!(pool.oracle, manifest.oracle);
    let feed_code = reader.get_code(manifest.oracle, &at).await.unwrap();
    let ratio_source = RatioNativeCostOracle::new(reader.clone(), NativeRatioFeedConfig {
        wrapped_native: wrapped_config, feed: manifest.oracle, feed_code_hash: keccak256(feed_code),
        asset: pool.asset, stale_window_seconds: 3600, margin_bps: 100,
    }).unwrap();
    assert!(matches!(ratio_source.quote_cost(manifest.asset, native_bound, &at).await,
        Err(EconomicsError::CostUnavailable(_))), "collateral/asset lending oracle is not a WMON/asset feed");
    reader.ensure_canonical(&at).await.expect("all reads retained the original block anchor");
}
