use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, B256, Bytes, U256};
use barterlend_liquidator::routes::{ExactOutputQuote, QuoteValidity, RouteKind, RouteLimits, RouteQuote, SearchQuality, V3CodeHashes, V3Config, encode_exact_input_path, encode_exact_output_path, uniswap_v3::compute_pool_address};
use barterlend_state::{BlockRef, ChainIdentity, Commitment};

fn address(byte: u8) -> Address { Address::repeat_byte(byte) }
fn block() -> BlockRef { BlockRef { number: 5, hash: B256::repeat_byte(5), parent_hash: B256::repeat_byte(4), timestamp: 50, commitment: Commitment::Finalized } }

fn config() -> V3Config {
    V3Config {
        chain: ChainIdentity { chain_id: 31337, genesis_hash: B256::repeat_byte(1) },
        factory: address(11), router: address(12), quoter: address(13), wrapped_native: address(14),
        code_hashes: V3CodeHashes { factory: B256::repeat_byte(11), router: B256::repeat_byte(12), quoter: B256::repeat_byte(13), wrapped_native: B256::repeat_byte(14) },
        allowed_tokens: BTreeSet::from([address(1), address(2), address(14)]),
        intermediates: vec![address(14)], fee_tiers: vec![500, 3000], pool_code_hashes: BTreeMap::new(), limits: RouteLimits::default(),
    }
}

fn quote() -> RouteQuote {
    let mut quote = RouteQuote {
        token_in: address(1), token_out: address(2), amount_in: U256::from(100), amount_out: U256::from(120), minimum_amount_out: U256::from(110),
        executor: address(9), router: Some(address(12)),
        route: RouteKind::V3 { path: encode_exact_input_path(&[address(1), address(2)], &[3000]).unwrap(), tokens: vec![address(1), address(2)], fees: vec![3000], pools: vec![address(20)] },
        validity: QuoteValidity::at(block(), 10, 3).unwrap(), quoter_gas_estimate: U256::from(10000),
        search: SearchQuality { candidates_examined: 1, quote_calls: 1, budget_exhausted: false }, fingerprint: B256::ZERO,
    };
    quote.fingerprint = quote.compute_fingerprint();
    quote
}

#[test]
fn forward_and_reverse_twohop_paths_match_independent_packed_bytes() {
    let tokens = [address(1), address(2), address(3)];
    let fees = [500, 3000];
    let mut forward = vec![1; 20]; forward.extend_from_slice(&[0, 1, 244]); forward.extend_from_slice(&[2; 20]); forward.extend_from_slice(&[0, 11, 184]); forward.extend_from_slice(&[3; 20]);
    let mut reverse = vec![3; 20]; reverse.extend_from_slice(&[0, 11, 184]); reverse.extend_from_slice(&[2; 20]); reverse.extend_from_slice(&[0, 1, 244]); reverse.extend_from_slice(&[1; 20]);
    assert_eq!(encode_exact_input_path(&tokens, &fees).unwrap(), Bytes::from(forward));
    assert_eq!(encode_exact_output_path(&tokens, &fees).unwrap(), Bytes::from(reverse));
    assert_eq!(encode_exact_input_path(&tokens[..2], &fees[..1]).unwrap().len(), 43);
    assert_eq!(encode_exact_output_path(&tokens, &fees).unwrap().len(), 66);
}

#[test]
fn invalid_lengths_cycles_zero_and_fee_bounds_are_rejected() {
    for (tokens, fees) in [
        (vec![address(1), address(1)], vec![500]),
        (vec![address(1), address(2), address(1)], vec![500, 3000]),
        (vec![Address::ZERO, address(2)], vec![500]),
        (vec![address(1), address(2)], vec![1_000_000]),
        (vec![address(1), address(2)], vec![500, 3000]),
        (vec![address(1), address(2), address(3), address(4)], vec![500, 3000, 10000]),
    ] { assert!(encode_exact_input_path(&tokens, &fees).is_err()); assert!(encode_exact_output_path(&tokens, &fees).is_err()); }
}

#[test]
fn create2_sorts_tokens_and_distinguishes_fee_factory() {
    let pool = compute_pool_address(address(11), address(1), address(2), 500).unwrap();
    assert_eq!(pool, compute_pool_address(address(11), address(2), address(1), 500).unwrap());
    assert_ne!(pool, compute_pool_address(address(11), address(1), address(2), 3000).unwrap());
    assert_ne!(pool, compute_pool_address(address(12), address(1), address(2), 500).unwrap());
    assert!(compute_pool_address(address(11), address(1), address(1), 500).is_err());
}

#[test]
fn quote_expiry_block_age_and_fork_boundaries_are_strict() {
    let validity = QuoteValidity::at(block(), 10, 3).unwrap();
    let mut now = block(); now.number = 8; now.timestamp = 59;
    validity.validate(&now).unwrap();
    now.timestamp = 60; assert!(validity.validate(&now).is_err());
    now.timestamp = 59; now.number = 9; assert!(validity.validate(&now).is_err());
    now = block(); now.hash = B256::repeat_byte(99); assert!(validity.validate(&now).is_err());
    now = block(); now.number = 4; assert!(validity.validate(&now).is_err());
    let mut last = block(); last.timestamp = u64::MAX;
    assert!(QuoteValidity::at(last, 1, 3).is_err());
}

#[test]
fn fingerprint_binds_output_input_executor_path_and_anchor() {
    let original = quote();
    original.validate(address(1), address(2), U256::from(100), address(9), &block()).unwrap();
    for field in 0..5 {
        let mut changed = original.clone();
        match field {
            0 => changed.amount_out += U256::from(1),
            1 => changed.amount_in += U256::from(1),
            2 => changed.executor = address(8),
            3 => if let RouteKind::V3 { path, .. } = &mut changed.route { *path = Bytes::from(vec![0; 43]); },
            4 => changed.validity.as_of.hash = B256::repeat_byte(88),
            _ => unreachable!(),
        }
        assert_ne!(changed.compute_fingerprint(), original.fingerprint);
        assert!(changed.validate(address(1), address(2), U256::from(100), address(9), &block()).is_err());
    }
}

#[test]
fn exact_output_guard_cannot_reuse_forward_path() {
    let input = quote();
    let mut output = ExactOutputQuote {
        token_in: input.token_in, token_out: input.token_out, amount_in: input.amount_in, amount_out: input.amount_out,
        maximum_amount_in: U256::from(101), executor: input.executor, router: input.router,
        route: input.route.clone(), reverse_path: input.route.exact_output_path().unwrap(), validity: input.validity,
        quoter_gas_estimate: input.quoter_gas_estimate, search: input.search, fingerprint: B256::ZERO,
    };
    output.fingerprint = output.compute_fingerprint();
    output.validate(address(1), address(2), U256::from(120), address(9), &block()).unwrap();
    output.reverse_path = input.route.exact_input_path(); output.fingerprint = output.compute_fingerprint();
    assert!(output.validate(address(1), address(2), U256::from(120), address(9), &block()).is_err());
}

#[test]
fn same_token_quotes_have_no_router_and_exact_raw_output() {
    let mut quote = quote();
    quote.token_out = quote.token_in; quote.amount_out = quote.amount_in; quote.minimum_amount_out = quote.amount_in;
    quote.router = None; quote.route = RouteKind::SameToken; quote.fingerprint = quote.compute_fingerprint();
    quote.validate(address(1), address(1), U256::from(100), address(9), &block()).unwrap();
    assert!(quote.route.exact_input_path().is_empty()); assert!(quote.route.exact_output_path().unwrap().is_empty());
    quote.router = Some(address(12)); quote.fingerprint = quote.compute_fingerprint();
    assert!(quote.validate(address(1), address(1), U256::from(100), address(9), &block()).is_err());
}

#[test]
fn config_rejects_missing_identity_unknown_intermediates_and_unbounded_search() {
    config().validate().unwrap();
    for field in 0..5 {
        let mut bad = config();
        match field {
            0 => bad.code_hashes.router = B256::ZERO,
            1 => bad.intermediates = vec![address(99)],
            2 => bad.fee_tiers = vec![500, 500],
            3 => bad.limits.max_candidates = usize::MAX,
            4 => bad.limits.slippage_bps = 10000,
            _ => unreachable!(),
        }
        assert!(bad.validate().is_err());
    }
}


#[test]
fn configured_paths_are_bounded_unverified_hints_and_need_no_rpc() {
    let reader = std::sync::Arc::new(barterlend_state::HttpReader::new("http://127.0.0.1:1", Default::default()).unwrap());
    let provider = barterlend_liquidator::routes::V3RouteProvider::new(reader.clone(), config()).unwrap();
    let routes = provider.configured_routes(address(1), address(2)).unwrap();
    assert_eq!(routes.len(), 6, "two direct paths and four two-hop fee combinations");
    for route in &routes {
        if let RouteKind::V3 { path, tokens, fees, pools } = route {
            assert_eq!(*path, encode_exact_input_path(tokens, fees).unwrap());
            for ((pair, &fee), &pool) in tokens.windows(2).zip(fees).zip(pools) {
                assert_eq!(pool, compute_pool_address(address(11), pair[0], pair[1], fee).unwrap());
            }
        } else { panic!("different tokens require a swap path"); }
    }
    assert_eq!(provider.configured_routes(address(1), address(1)).unwrap(), vec![RouteKind::SameToken]);
    assert!(provider.configured_routes(address(1), address(99)).is_err());
    let mut config = config(); config.limits.max_candidates = 1;
    let provider = barterlend_liquidator::routes::V3RouteProvider::new(reader, config).unwrap();
    assert_eq!(provider.configured_routes(address(1), address(2)).unwrap().len(), 6);
}
