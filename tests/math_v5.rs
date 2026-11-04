use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol_types::SolValue;
use barterlend_state::{*, abi::{ABI_FINGERPRINT, PROTOCOL_VERSION}, risk::evaluate_risk};
use barterlend_liquidator::{
    liquidation_math::{LiquidationInput, LiquidationMathError, UNIT, exact_amounts, mul_div_ceil, mul_div_floor, remainder_repay_limit},
    planner::{PlannerConfig, plan_position},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ActualExecution {
    name: String,
    debt: String,
    collateral: String,
    requested: String,
    minimum_remaining: String,
    numerator: String,
    denominator: String,
    discount: String,
    maximum_discount: String,
    ok: bool,
    actual_repay: String,
    seized: String,
    remaining_debt: String,
    remaining_collateral: String,
    bad_debt: String,
    revert_data: String,
}

fn uint(text: &str) -> U256 { U256::from_str_radix(text, 10).unwrap() }

fn input(requested_debt: u128) -> LiquidationInput {
    LiquidationInput { debt: 100 * UNIT, collateral: 250 * UNIT, requested_debt,
        discount: U256::from(UNIT / 5), max_discount: U256::from(UNIT / 5), min_debt_remaining: 1,
        price: Price { numerator: U256::from(44), denominator: U256::from(100), updated_at: U256::from(1) } }
}

#[test]
fn actual_solidity_execution_corpus_matches_rust_without_expected_math_copy() {
    let vectors: Vec<ActualExecution> = serde_json::from_str(include_str!("fixtures/math-v5/generated/actual-liquidations.json")).unwrap();
    assert_eq!(vectors.len(), 141);
    for vector in &vectors { assert_actual_execution(vector); }
}

fn assert_actual_execution(vector: &ActualExecution) {
    let input = LiquidationInput { debt: vector.debt.parse().unwrap(), collateral: vector.collateral.parse().unwrap(),
        requested_debt: vector.requested.parse().unwrap(), discount: uint(&vector.discount),
        max_discount: uint(&vector.maximum_discount),
        price: Price { numerator: uint(&vector.numerator), denominator: uint(&vector.denominator), updated_at: U256::from(1) },
        min_debt_remaining: vector.minimum_remaining.parse().unwrap() };
    match exact_amounts(&input) {
        Ok(amounts) => {
            assert!(vector.ok, "{} Rust accepted Solidity revert {}", vector.name, vector.revert_data);
            assert_eq!(amounts.actual_repay, vector.actual_repay.parse::<u128>().unwrap(), "{} repayment", vector.name);
            assert_eq!(amounts.seized_collateral, vector.seized.parse::<u128>().unwrap(), "{} seizure", vector.name);
            assert_eq!(amounts.remaining_debt, vector.remaining_debt.parse::<u128>().unwrap(), "{} debt", vector.name);
            assert_eq!(amounts.remaining_collateral, vector.remaining_collateral.parse::<u128>().unwrap(), "{} collateral", vector.name);
            assert_eq!(amounts.bad_debt, vector.bad_debt.parse::<u128>().unwrap(), "{} bad debt", vector.name);
            assert_eq!(amounts.drains_collateral, amounts.seized_collateral == input.collateral);
        },
        Err(error) => {
            assert!(!vector.ok, "{} Rust rejected Solidity success: {error}", vector.name);
            let signature = match error {
                LiquidationMathError::AmountOutOfRange { .. } => "AmountIsOutOfRange(uint256,uint256,uint256)",
                LiquidationMathError::NoDebtToLiquidate => "NoDebtToLiquidate()",
                LiquidationMathError::NoCollateralToSeize => "NoCollateralToSeize()",
                LiquidationMathError::InsufficientLiquidationAmount { .. } => "InsufficientLiquidationAmount(uint256,uint256,uint256)",
                _ => panic!("{} unexpected error: {error}", vector.name),
            };
            let selector = format!("0x{}", alloy::hex::encode(&keccak256(signature)[..4]));
            assert!(vector.revert_data.starts_with(&selector), "{} wrong Solidity revert {} vs {signature}", vector.name, vector.revert_data);
            if let LiquidationMathError::InsufficientLiquidationAmount {
                remaining_debt, remaining_collateral_value, minimum_remaining,
            } = error {
                let payload = (U256::from(remaining_debt), remaining_collateral_value, U256::from(minimum_remaining)).abi_encode();
                assert_eq!(vector.revert_data, format!("{selector}{}", alloy::hex::encode(payload)),
                    "{} remainder error arguments", vector.name);
            }
        },
    }
}

#[test]
#[ignore = "requires actual temporary Solidity fuzz corpus; use tests/runners/test-math-differential.py"]
fn actual_solidity_fuzz_corpus_matches_rust() {
    let path = std::env::var("BARTERLEND_MATH_FUZZ_CORPUS").expect("explicit local corpus required");
    let expected: usize = std::env::var("BARTERLEND_MATH_FUZZ_RUNS").expect("explicit run count required").parse().unwrap();
    assert!((1..=16_384).contains(&expected));
    let metadata = std::fs::metadata(&path).unwrap();
    assert!(metadata.is_file() && metadata.len() <= 32 * 1024 * 1024, "bounded regular local corpus required");
    let contents = std::fs::read_to_string(path).unwrap();
    let vectors: Vec<ActualExecution> = contents.lines().enumerate().map(|(index, line)| {
        let mut vector: ActualExecution = serde_json::from_str(line).unwrap();
        vector.name = format!("fuzz-{index}");
        vector
    }).collect();
    assert_eq!(vectors.len(), expected, "every actual Solidity fuzz run must reach the Rust comparator");
    for vector in &vectors { assert_actual_execution(vector); }
    println!("Compared {} actual Solidity fuzz executions with Rust repayment/seizure/remainders/bad-debt/error selectors", vectors.len());
}

#[test]
fn full_request_can_be_capped_and_socialize_bad_debt() {
    let amounts = exact_amounts(&input(100 * UNIT)).unwrap();
    assert_eq!(amounts.actual_repay, 88 * UNIT);
    assert_eq!(amounts.seized_collateral, 250 * UNIT);
    assert_eq!(amounts.bad_debt, 12 * UNIT);
    assert_eq!(amounts.remaining_debt, 0);
    assert_eq!(amounts.remaining_collateral, 0);
}

#[test]
fn recovery_equality_does_not_imply_drain() {
    let mut case = input(4);
    case.min_debt_remaining = 0;
    case.price.numerator = U256::from(6);
    case.price.denominator = U256::from(250 * UNIT);
    let amounts = exact_amounts(&case).unwrap();
    assert_eq!(amounts.max_recovery, U256::from(4));
    assert_eq!(amounts.actual_repay, 4);
    assert_eq!(amounts.seized_collateral, 208_333_333_333_333_333_334);
    assert!(!amounts.drains_collateral);
    assert_eq!(amounts.bad_debt, 0);
    case.requested_debt = 5;
    let capped = exact_amounts(&case).unwrap();
    assert_eq!(capped.actual_repay, 4);
    assert_eq!(capped.seized_collateral, 250 * UNIT);
    assert!(capped.drains_collateral);
}

#[test]
fn two_ceil_steps_are_not_replaced_by_one_rational_ceil() {
    let mut case = input(1);
    case.debt = 2;
    case.collateral = 3;
    case.price.numerator = U256::from(2);
    case.price.denominator = U256::from(3);
    let amounts = exact_amounts(&case).unwrap();
    assert_eq!(amounts.seized_collateral, 3); // ceil(ceil(3/2) / .8) = 3, combined ceil would be 2.
    assert_eq!(amounts.actual_repay, 1);
    assert_eq!(amounts.bad_debt, 1);
}

#[test]
fn dust_threshold_is_inclusive_and_drain_allows_subminimum_remainder() {
    let mut case = input(80 * UNIT);
    case.collateral = 300 * UNIT;
    case.min_debt_remaining = 20 * UNIT;
    assert_eq!(exact_amounts(&case).unwrap().remaining_debt, 20 * UNIT);
    case.requested_debt += 1;
    assert!(matches!(exact_amounts(&case), Err(LiquidationMathError::InsufficientLiquidationAmount { .. })));
    case.collateral = 250 * UNIT;
    case.requested_debt = 100 * UNIT;
    assert_eq!(exact_amounts(&case).unwrap().bad_debt, 12 * UNIT);
}

#[test]
fn remainder_uses_market_maximum_discount_and_reports_actual_remainder() {
    let mut case = input(750);
    case.debt = 1_000;
    case.collateral = 1_000;
    case.price.numerator = U256::from(1);
    case.price.denominator = U256::from(1);
    case.discount = U256::ZERO;
    case.max_discount = U256::from(UNIT / 5);
    case.min_debt_remaining = 200;
    let boundary = exact_amounts(&case).unwrap();
    assert_eq!((boundary.remaining_debt, boundary.remaining_collateral), (250, 250));
    assert_eq!(remainder_repay_limit(&case).unwrap(), 750);
    case.requested_debt = 751;
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InsufficientLiquidationAmount {
        remaining_debt: 249, remaining_collateral_value: U256::from(249), minimum_remaining: 200,
    }));
    case.min_debt_remaining = 0;
    assert_eq!(exact_amounts(&case).unwrap().actual_repay, 751);
}

#[test]
fn remainder_valuation_preserves_both_floor_roundings() {
    let mut case = input(1);
    case.debt = 3;
    case.collateral = 4;
    case.price.numerator = U256::from(2);
    case.price.denominator = U256::from(3);
    case.discount = U256::ZERO;
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InsufficientLiquidationAmount {
        remaining_debt: 2, remaining_collateral_value: U256::from(1), minimum_remaining: 1,
    }));
    assert_eq!(remainder_repay_limit(&case).unwrap(), 0);
    // floor(floor(2 * 2 / 3) * .8) = 0, while a single combined floor gives 1.
    case.requested_debt = 3;
    let capped = exact_amounts(&case).unwrap();
    assert_eq!((capped.actual_repay, capped.bad_debt, capped.remaining_collateral), (2, 1, 0));
}

#[test]
fn full_repayment_is_exempt_even_when_residual_collateral_cannot_cover_minimum() {
    let mut case = input(10);
    case.debt = 10;
    case.collateral = 100;
    case.price.numerator = U256::from(1);
    case.price.denominator = U256::from(1);
    case.min_debt_remaining = 1_000;
    let amounts = exact_amounts(&case).unwrap();
    assert_eq!(amounts.remaining_debt, 0);
    assert_eq!(amounts.remaining_collateral, 87);
    assert_eq!(remainder_repay_limit(&case).unwrap(), 0);
}

#[test]
fn invalid_maximum_discount_is_explicit_and_tiny_price_does_not_overflow_boundary() {
    let mut case = input(1);
    case.max_discount = U256::from(UNIT);
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InvalidDiscount));
    assert_eq!(remainder_repay_limit(&case), Err(LiquidationMathError::InvalidDiscount));
    case.max_discount = U256::from(UNIT / 10);
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InvalidDiscount));
    case.max_discount = U256::from(UNIT / 5);
    case.price.numerator = U256::from(1);
    case.price.denominator = U256::MAX;
    assert_eq!(remainder_repay_limit(&case).unwrap(), 0);
}

#[test]
fn exact_remainder_boundary_matches_exhaustive_small_amount_search() {
    for debt in [3, 9, 17, 31] {
        for collateral in [2, 5, 11, 29] {
            for (numerator, denominator) in [(1, 1), (2, 3), (5, 2), (7, 11)] {
                for minimum in [1, 2, 7, 20] {
                    for discount in [0, UNIT / 10, UNIT / 5] {
                        let mut case = input(1);
                        case.debt = debt;
                        case.collateral = collateral;
                        case.price.numerator = U256::from(numerator);
                        case.price.denominator = U256::from(denominator);
                        case.min_debt_remaining = minimum;
                        case.discount = U256::from(discount);
                        case.max_discount = U256::from(UNIT * 3 / 10);
                        let expected = (1..debt).filter(|requested| {
                            let probe = LiquidationInput { requested_debt: *requested, ..case.clone() };
                            matches!(exact_amounts(&probe), Ok(amounts) if !amounts.drains_collateral)
                        }).max().unwrap_or(0);
                        assert_eq!(remainder_repay_limit(&case).unwrap(), expected, "{case:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn amount_sentinel_zero_recovery_and_invalid_price_are_explicit() {
    assert_eq!(exact_amounts(&input(u128::MAX)).unwrap(), exact_amounts(&input(100 * UNIT)).unwrap());
    assert!(matches!(exact_amounts(&input(0)), Err(LiquidationMathError::AmountOutOfRange { .. })));
    assert!(matches!(exact_amounts(&input(100 * UNIT + 1)), Err(LiquidationMathError::AmountOutOfRange { .. })));
    let mut case = input(1);
    case.price.numerator = U256::from(1);
    case.price.denominator = U256::from(250 * UNIT);
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::NoDebtToLiquidate));
    case.collateral = 0;
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::NoCollateralToSeize));
    case.collateral = 1;
    case.price.denominator = U256::ZERO;
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InvalidPrice));
    case.price.denominator = U256::from(1);
    case.discount = U256::from(UNIT);
    assert_eq!(exact_amounts(&case), Err(LiquidationMathError::InvalidDiscount));
}

#[test]
fn full_precision_muldiv_accepts_intermediate_overflow_and_checks_result() {
    assert_eq!(mul_div_floor(U256::MAX, U256::MAX, U256::MAX).unwrap(), U256::MAX);
    assert_eq!(mul_div_ceil(U256::MAX - U256::from(1), U256::MAX - U256::from(1), U256::MAX).unwrap(), U256::MAX - U256::from(1));
    assert_eq!(mul_div_floor(U256::MAX, U256::MAX, U256::from(1)), Err(LiquidationMathError::ArithmeticOverflow));
    assert_eq!(mul_div_ceil(U256::from(1), U256::from(1), U256::from(1_000)).unwrap(), U256::from(1));
}

#[test]
fn price_fraction_already_contains_18_to_6_and_8_to_6_decimal_scaling() {
    for (collateral, denominator) in [(250 * UNIT, 100_000_000_000_000_u128), (25_000_000_000, 10_000)] {
        let mut case = input(100_000_000);
        case.debt = 100_000_000;
        case.collateral = collateral;
        case.price.numerator = U256::from(44);
        case.price.denominator = U256::from(denominator);
        let amounts = exact_amounts(&case).unwrap();
        assert_eq!(amounts.collateral_value, U256::from(110_000_000));
        assert_eq!(amounts.actual_repay, 88_000_000);
        assert_eq!(amounts.seized_collateral, collateral);
        assert_eq!(amounts.bad_debt, 12_000_000);
    }
}

fn snapshot(debt: u128, collateral: u128, price: Price) -> (StateSnapshot, PositionKey) {
    let pool = Address::repeat_byte(1);
    let key = PositionKey { pool, market: 1_000, borrower: Address::repeat_byte(2) };
    let globals = PoolRiskConfig { liquidation_ltv: U256::from(UNIT / 2), stale_window: 60, min_debt_remaining: 10 * UNIT };
    let market = MarketRiskConfig { grace_period: 100, discount_min: U256::from(UNIT / 20), discount_max: U256::from(UNIT / 5) };
    let oracle = OracleStatus::Ready(price);
    let as_of = BlockRef { number: 7, hash: B256::repeat_byte(7), parent_hash: B256::repeat_byte(6), timestamp: 1_000, commitment: Commitment::Finalized };
    let amounts = PositionAmounts { debt, collateral };
    let risk = evaluate_risk(&amounts, key.market, &globals, &market, &oracle, as_of.timestamp);
    (StateSnapshot {
        identity: ChainIdentity { chain_id: 143, genesis_hash: B256::repeat_byte(3) },
        spec: PoolSpec { address: pool, deployment_block: 0, expected_version: PROTOCOL_VERSION, deployment_block_hash: Some(B256::repeat_byte(3)) },
        pool: PoolDescriptor { address: pool, version: PROTOCOL_VERSION,
            asset: TokenDescriptor { address: Address::repeat_byte(4), decimals: 6 },
            collateral: TokenDescriptor { address: Address::repeat_byte(5), decimals: 18 },
            oracle: Address::repeat_byte(6), code_hash: B256::repeat_byte(8) },
        abi_fingerprint: ABI_FINGERPRINT, epoch: 1, as_of, globals,
        markets: BTreeMap::from([(key.market, market)]), oracle,
        positions: BTreeMap::from([(key, PositionRecord { key, amounts, risk, last_changed: as_of })]),
    }, key)
}

fn refresh_risk(snapshot: &mut StateSnapshot, key: PositionKey) {
    let record = snapshot.positions.get_mut(&key).unwrap();
    record.risk = evaluate_risk(&record.amounts, key.market, &snapshot.globals, &snapshot.markets[&key.market], &snapshot.oracle, snapshot.as_of.timestamp);
}

fn default_price() -> Price {
    Price { numerator: U256::from(44), denominator: U256::from(100), updated_at: U256::from(1_000) }
}

#[test]
fn planner_keeps_capped_and_partial_candidates_and_deduplicates_actual_amounts() {
    let (snapshot, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    let plans = plan_position(&snapshot, &key, &PlannerConfig::default()).unwrap();
    assert_eq!(plans[0].requested_debt, 100 * UNIT);
    assert_eq!(plans[0].expected.actual_repay, 88 * UNIT);
    assert!(plans.iter().any(|plan| plan.requested_debt == 50 * UNIT && plan.expected.actual_repay == 50 * UNIT));
    let distinct: BTreeSet<_> = plans.iter().map(|plan| (plan.expected.actual_repay, plan.expected.seized_collateral)).collect();
    assert_eq!(distinct.len(), plans.len());
    assert!(plans.len() <= PlannerConfig::default().max_candidates);
    assert!(plans.iter().all(|plan| plan.requested_debt != u128::MAX && plan.belongs_to_snapshot(&snapshot)));
    let limited = PlannerConfig { max_candidates: 1, ..Default::default() };
    assert_eq!(plan_position(&snapshot, &key, &limited).unwrap().len(), 1);
    // Nonmonotone venue economics may make only the partial quote acceptable.
    let accepted_partial = plans.iter().find(|plan| plan.requested_debt == 50 * UNIT).unwrap();
    assert!(!accepted_partial.expected.drains_collateral);
}

#[test]
fn planner_includes_recoverable_remainder_boundary_and_skips_zero_recovery() {
    let (mut state, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    state.globals.min_debt_remaining = 20 * UNIT;
    let plans = plan_position(&state, &key, &PlannerConfig::default()).unwrap();
    let boundary = plans.iter().find(|plan| plan.requested_debt == 67_999_999_999_999_999_999).expect("exact collateral recovery boundary");
    assert_eq!(boundary.expected.remaining_debt, 32_000_000_000_000_000_001);
    assert!(!plans.iter().any(|plan| plan.requested_debt == 80 * UNIT), "old debt-only boundary must be rejected");
    state.oracle = OracleStatus::Ready(Price { numerator: U256::from(1), denominator: U256::from(250 * UNIT), updated_at: U256::from(1_000) });
    refresh_risk(&mut state, key);
    assert!(plan_position(&state, &key, &PlannerConfig::default()).unwrap().is_empty());
}

#[test]
fn funding_planner_rejects_future_oracle_even_when_protocol_risk_is_ready() {
    let (mut state, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    assert!(!plan_position(&state, &key, &PlannerConfig::default()).unwrap().is_empty());
    if let OracleStatus::Ready(ref mut price) = state.oracle {
        price.updated_at = U256::from(state.as_of.timestamp) + U256::from(1);
    }
    refresh_risk(&mut state, key);
    assert!(matches!(&state.positions[&key].risk, RiskStatus::Ready(risk) if risk.eligible),
        "SDK deliberately preserves protocol acceptance of a future timestamp");
    let error = plan_position(&state, &key, &PlannerConfig::default())
        .expect_err("execution planner must reject future oracle data even if protocol risk is ready");
    assert!(error.to_string().contains("oracle timestamp is in the future"));
}

#[test]
fn planner_bounds_deadline_to_last_fresh_second_and_rejects_unavailable_inputs() {
    let (mut state, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    state.globals.stale_window = 20;
    if let OracleStatus::Ready(ref mut price) = state.oracle { price.updated_at = U256::from(990); }
    refresh_risk(&mut state, key);
    let plans = plan_position(&state, &key, &PlannerConfig::default()).unwrap();
    assert!(plans.iter().all(|plan| plan.deadline == 1_009));
    let mut reset = state.clone(); reset.epoch += 1;
    assert!(!plans[0].belongs_to_snapshot(&reset));
    state.oracle = OracleStatus::Unavailable(OracleFailure::Rpc("offline".into()));
    refresh_risk(&mut state, key);
    assert!(plan_position(&state, &key, &PlannerConfig::default()).is_err());
}

#[test]
fn planner_does_not_emit_max_sentinel_even_at_uint128_max_debt() {
    let (state, key) = snapshot(u128::MAX, u128::MAX,
        Price { numerator: U256::from(1), denominator: U256::from(1), updated_at: U256::from(1_000) });
    let plans = plan_position(&state, &key, &PlannerConfig::default()).unwrap();
    assert!(!plans.is_empty());
    assert_eq!(plans[0].requested_debt, u128::MAX - 1);
    assert!(plans.iter().all(|plan| plan.requested_debt > 0 && plan.requested_debt < u128::MAX));
}

#[test]
fn planner_rejects_corrupt_risk_and_invalid_resource_settings() {
    let (mut state, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    if let RiskStatus::Ready(ref mut risk) = state.positions.get_mut(&key).unwrap().risk { risk.discount = U256::ZERO; }
    assert!(plan_position(&state, &key, &PlannerConfig::default()).is_err());
    for config in [PlannerConfig { max_candidates: 0, ..Default::default() },
        PlannerConfig { max_candidates: 65, ..Default::default() },
        PlannerConfig { partial_divisors: vec![1], ..Default::default() },
        PlannerConfig { ttl_seconds: 0, ..Default::default() }]
    {
        assert!(config.validate().is_err());
    }
}

#[test]
fn paid_state_fingerprint_is_stable_across_q_and_blocks_but_changes_with_actual_amounts() {
    let (state, key) = snapshot(100 * UNIT, 250 * UNIT, default_price());
    let plans = plan_position(&state, &key, &PlannerConfig::default()).unwrap();
    let full = &plans[0];
    let partial = plans.iter().find(|plan| plan.requested_debt == 50 * UNIT).unwrap();
    assert_ne!(full.expected.actual_repay, partial.expected.actual_repay);
    assert_ne!(full.expected.seized_collateral, partial.expected.seized_collateral);
    assert_eq!(full.paid_state_fingerprint().unwrap(), partial.paid_state_fingerprint().unwrap());
    let mut reanchored = partial.clone();
    reanchored.as_of.number += 1;
    reanchored.as_of.hash = B256::repeat_byte(9);
    reanchored.as_of.timestamp += 1;
    reanchored.deadline += 1;
    assert_eq!(partial.paid_state_fingerprint().unwrap(), reanchored.paid_state_fingerprint().unwrap());
    let (changed_debt, key) = snapshot(99 * UNIT, 250 * UNIT, default_price());
    let changed = plan_position(&changed_debt, &key, &PlannerConfig::default()).unwrap();
    assert_ne!(full.paid_state_fingerprint().unwrap(), changed[0].paid_state_fingerprint().unwrap());
    let (changed_collateral, key) = snapshot(100 * UNIT, 249 * UNIT, default_price());
    let changed = plan_position(&changed_collateral, &key, &PlannerConfig::default()).unwrap();
    assert_ne!(full.paid_state_fingerprint().unwrap(), changed[0].paid_state_fingerprint().unwrap());
    let mut overflow = full.clone();
    overflow.expected.actual_repay = u128::MAX;
    overflow.expected.remaining_debt = 1;
    assert!(overflow.paid_state_fingerprint().is_err());
}
