use alloy::primitives::{Address, B256, U256};
use barterlend_liquidator::{economics::{CostQuote, EconomicsError, FeePolicy, LegacyFeePlan, NativeCostQuality, apply_margin, build_expected_economics, charged_native, ratio_cost}, routes::QuoteValidity};
use barterlend_state::{BlockRef, Commitment};

fn block() -> BlockRef { BlockRef { number: 5, hash: B256::repeat_byte(5), parent_hash: B256::repeat_byte(4), timestamp: 50, commitment: Commitment::Finalized } }
fn asset() -> Address { Address::repeat_byte(1) }
fn policy() -> FeePolicy { FeePolicy { gas_margin_bps: 1000, max_gas_limit: 1_000_000, max_gas_price: U256::from(100), max_fee_native: U256::from(100_000_000), native_reserve: U256::from(100_000) } }
fn cost(native_bound: U256, cost_asset: U256) -> CostQuote {
    let mut quote = CostQuote { asset: asset(), native_bound, cost_asset, validity: QuoteValidity::at(block(), 10, 3).unwrap(),
        quality: NativeCostQuality::WrappedIdentity { wrapped_native: asset(), code_hash: B256::repeat_byte(7) }, fingerprint: B256::ZERO };
    quote.fingerprint = quote.compute_fingerprint(); quote
}

#[test]
fn signed_padded_limit_is_charged_and_reserve_equality_is_allowed() {
    let fee = policy().plan_legacy(100_001, U256::from(3), U256::from(430_006)).unwrap();
    assert_eq!(fee.signed_gas_limit, 110_002);
    assert_eq!(fee.fee_bound_native, U256::from(330_006));
    assert_eq!(charged_native(fee.signed_gas_limit, U256::from(3)).unwrap(), U256::from(330_006));
    assert_ne!(fee.fee_bound_native, U256::from(100_001 * 3));
    assert!(matches!(policy().plan_legacy(100_001, U256::from(3), U256::from(430_005)), Err(EconomicsError::InsufficientNative)));
}

#[test]
fn estimation_probe_obeys_balance_reserve_fee_and_price_caps_without_rounding_up() {
    let mut limits = policy();
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::from(430_006)).unwrap(), 110_002);
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::from(430_005)).unwrap(), 110_001);
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::MAX).unwrap(), limits.max_gas_limit);
    assert!(limits.estimation_probe_limit(U256::ZERO, U256::MAX).is_err());
    assert!(matches!(limits.estimation_probe_limit(U256::from(101), U256::MAX), Err(EconomicsError::Budget(_))));
    limits.max_fee_native = U256::from(100_001);
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::MAX).unwrap(), 33_333);
    limits.max_gas_limit = 25_000;
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::MAX).unwrap(), 25_000);
    assert_eq!(limits.estimation_probe_limit(U256::from(3), U256::from(163_000)).unwrap(), 21_000);
    assert!(matches!(limits.estimation_probe_limit(U256::from(3), U256::from(162_999)), Err(EconomicsError::InsufficientNative)));
    assert!(matches!(limits.estimation_probe_limit(U256::from(3), U256::from(9)), Err(EconomicsError::InsufficientNative)));
    limits.max_fee_native = U256::from(62_999);
    assert!(matches!(limits.estimation_probe_limit(U256::from(3), U256::MAX), Err(EconomicsError::Budget(_))));
}

#[test]
fn fee_caps_reject_instead_of_clamping_or_zero_cost() {
    let mut policy = policy(); policy.max_gas_limit = 100;
    assert!(matches!(policy.plan_legacy(100, U256::from(1), U256::MAX), Err(EconomicsError::Budget(_))));
    policy.max_gas_limit = 1_000_000; policy.max_fee_native = U256::from(100);
    assert!(matches!(policy.plan_legacy(100, U256::from(1), U256::MAX), Err(EconomicsError::Budget(_))));
    assert!(policy.plan_legacy(1, U256::from(101), U256::MAX).is_err());
    assert!(policy.plan_legacy(0, U256::from(1), U256::MAX).is_err());
    assert!(policy.plan_legacy(1, U256::ZERO, U256::MAX).is_err());
    policy.max_gas_limit = 30_000_001; assert!(policy.validate().is_err());
}

#[test]
fn ratio_ceil_preserves_subunit_cost_without_double_decimals_scaling() {
    assert_eq!(ratio_cost(U256::from(1), U256::from(1), U256::from(1_000_000_000_000u64)).unwrap(), U256::from(1));
    assert_eq!(ratio_cost(U256::from(1_000_000_000_000_000_000u64), U256::from(1), U256::from(1_000_000_000_000u64)).unwrap(), U256::from(1_000_000));
    assert_eq!(apply_margin(U256::from(1), 1).unwrap(), U256::from(2));
    assert!(ratio_cost(U256::from(1), U256::ZERO, U256::from(1)).is_err());
    assert!(ratio_cost(U256::from(1), U256::from(1), U256::ZERO).is_err());
}

#[test]
fn wide_product_is_valid_but_unrepresentable_result_and_fee_overflow_fail() {
    assert_eq!(ratio_cost(U256::MAX, U256::MAX, U256::MAX).unwrap(), U256::MAX);
    assert!(ratio_cost(U256::MAX, U256::MAX, U256::from(1)).is_err());
    assert!(charged_native(2, U256::MAX).is_err());
    assert!(apply_margin(U256::MAX, 1).is_err());
}

#[test]
fn fee_and_native_quote_must_match_exact_envelope_and_target() {
    let fee = LegacyFeePlan { signed_gas_limit: 1, gas_price: U256::from(1), fee_bound_native: U256::from(1) };
    let expected = build_expected_economics(asset(), fee.clone(), cost(U256::from(1), U256::from(3)), U256::from(2), &block()).unwrap();
    assert_eq!(expected.min_asset_profit, U256::from(5));
    let mut altered = fee.clone(); altered.fee_bound_native = U256::from(2);
    assert!(build_expected_economics(asset(), altered, cost(U256::from(2), U256::from(3)), U256::ZERO, &block()).is_err());
    assert!(build_expected_economics(asset(), fee.clone(), cost(U256::from(2), U256::from(3)), U256::ZERO, &block()).is_err());
    let mut now = block(); now.number += 1; now.timestamp += 1;
    assert!(build_expected_economics(asset(), fee, cost(U256::from(1), U256::from(3)), U256::ZERO, &now).is_err());
}

#[test]
fn zero_unknown_tampered_and_expired_cost_are_not_profit_guards() {
    let fee = LegacyFeePlan { signed_gas_limit: 1, gas_price: U256::from(1), fee_bound_native: U256::from(1) };
    assert!(build_expected_economics(asset(), fee.clone(), cost(U256::from(1), U256::ZERO), U256::ZERO, &block()).is_err());
    let mut changed = cost(U256::from(1), U256::from(2)); changed.cost_asset = U256::from(1);
    assert!(build_expected_economics(asset(), fee.clone(), changed, U256::ZERO, &block()).is_err());
    let mut expired = block(); expired.timestamp += 10;
    assert!(cost(U256::from(1), U256::from(2)).validate(asset(), U256::from(1), &expired).is_err());
    assert!(build_expected_economics(Address::repeat_byte(2), fee, cost(U256::from(1), U256::from(2)), U256::ZERO, &block()).is_err());
}

#[test]
fn asset_margin_overflow_and_reserve_overflow_fail_closed() {
    let fee = LegacyFeePlan { signed_gas_limit: 1, gas_price: U256::from(1), fee_bound_native: U256::from(1) };
    assert!(build_expected_economics(asset(), fee, cost(U256::from(1), U256::MAX), U256::from(1), &block()).is_err());
    let mut policy = policy(); policy.native_reserve = U256::MAX;
    assert!(matches!(policy.plan_legacy(1, U256::from(1), U256::MAX), Err(EconomicsError::Arithmetic(_))));
}

#[test]
fn zero_additional_profit_keeps_the_full_native_cost_in_the_asset_guard() {
    let fee = LegacyFeePlan { signed_gas_limit: 100_000, gas_price: U256::from(3), fee_bound_native: U256::from(300_000) };
    let expected = build_expected_economics(asset(), fee, cost(U256::from(300_000), U256::from(17)), U256::ZERO, &block()).unwrap();
    assert_eq!(expected.min_asset_profit, U256::from(17));
    assert_eq!(expected.gas_cost_asset, U256::from(17));
    assert_eq!(expected.fee_bound_native, U256::from(300_000));
}
