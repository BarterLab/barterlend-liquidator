//! Bounded deterministic full, capped and partial funding candidates.
//! Quotes, gas estimates and final simulation are handled by subsequent stages.

use std::collections::BTreeSet;

use alloy::primitives::U256;
use barterlend_state::{BlockRef, OracleStatus, PositionKey, RiskStatus, StateSnapshot, TokenDescriptor,
    abi::{ABI_FINGERPRINT, PROTOCOL_VERSION}, risk::evaluate_risk};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::liquidation_math::{LiquidationAmounts, LiquidationInput, LiquidationMathError, checked_u128, exact_amounts, remainder_repay_limit};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerConfig {
    /// Maximum distinct repay/seizure pairs sent downstream for quotes, 1..=64.
    pub max_candidates: usize,
    /// D/divisor probes, ordered, independent of route-profit monotonicity.
    pub partial_divisors: Vec<u32>,
    /// Chain time, never the process wall clock. Protocol price expiry also applies.
    pub ttl_seconds: u64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self { max_candidates: 12, partial_divisors: vec![2, 3, 4, 8, 16], ttl_seconds: 60 }
    }
}

impl PlannerConfig {
    pub fn validate(&self) -> Result<()> {
        if !(1..=64).contains(&self.max_candidates) || self.partial_divisors.len() > 64
            || self.partial_divisors.iter().any(|divisor| *divisor < 2) || self.ttl_seconds == 0
        {
            return Err(eyre!("invalid planner budget/divisors/TTL"));
        }
        Ok(())
    }
}

/// Funding only: route, fee guard and exact-calldata fingerprint are added by
/// the quote/simulation stage. A plan cannot authorize signing by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingPlan {
    pub key: PositionKey,
    pub epoch: u64,
    pub as_of: BlockRef,
    pub asset: TokenDescriptor,
    pub collateral: TokenDescriptor,
    /// Concrete q, never uint128::MAX (the protocol interprets MAX as current D).
    pub requested_debt: u128,
    pub expected: LiquidationAmounts,
    pub min_discount_out: U256,
    /// Inclusive Solidity deadline in chain seconds.
    pub deadline: u64,
}

impl FundingPlan {
    /// Stable across q/route/block retries, changes with borrower amounts or liquidation inputs.
    pub fn paid_state_fingerprint(&self) -> Result<alloy::primitives::B256> {
        let debt = self.expected.actual_repay.checked_add(self.expected.remaining_debt).and_then(|v| v.checked_add(self.expected.bad_debt)).ok_or_else(|| eyre!("funding debt reconstruction overflow"))?;
        let collateral = self.expected.seized_collateral.checked_add(self.expected.remaining_collateral).ok_or_else(|| eyre!("funding collateral reconstruction overflow"))?;
        Ok(alloy::primitives::keccak256(serde_json::to_vec(&(self.key,self.epoch,self.asset,self.collateral,debt,collateral,self.expected.collateral_value,self.expected.max_recovery,self.min_discount_out))?))
    }
    pub fn belongs_to_snapshot(&self, snapshot: &StateSnapshot) -> bool {
        self.key.pool == snapshot.pool.address && self.epoch == snapshot.epoch && self.as_of == snapshot.as_of
            && self.asset == snapshot.pool.asset && self.collateral == snapshot.pool.collateral
    }
}

pub fn plan_position(snapshot: &StateSnapshot, key: &PositionKey, config: &PlannerConfig) -> Result<Vec<FundingPlan>> {
    config.validate()?;
    if snapshot.pool.version != PROTOCOL_VERSION || snapshot.spec.expected_version != PROTOCOL_VERSION
        || snapshot.abi_fingerprint != ABI_FINGERPRINT || snapshot.pool.address != snapshot.spec.address
        || key.pool != snapshot.pool.address || snapshot.epoch == 0 || snapshot.as_of.hash.is_zero()
    {
        return Err(eyre!("invalid snapshot identity/version for funding planner"));
    }
    let record = snapshot.positions.get(key).ok_or_else(|| eyre!("position absent from current snapshot"))?;
    if record.key != *key { return Err(eyre!("snapshot position key mismatch")); }
    let market = snapshot.markets.get(&key.market).ok_or_else(|| eyre!("effective market config is missing"))?;
    let status = evaluate_risk(&record.amounts, key.market, &snapshot.globals, market,
        &snapshot.oracle, snapshot.as_of.timestamp);
    if record.risk != status { return Err(eyre!("snapshot risk differs from deterministic current inputs")); }
    let risk = match status {
        RiskStatus::Ready(risk) => risk,
        RiskStatus::Unavailable(reason) => return Err(eyre!("risk unavailable: {reason:?}")),
    };
    if !risk.eligible || record.amounts.debt == 0 || record.amounts.collateral == 0 { return Ok(Vec::new()); }
    let price = match snapshot.oracle {
        OracleStatus::Ready(price) => price,
        OracleStatus::Unavailable(ref reason) => return Err(eyre!("oracle unavailable: {reason:?}")),
    };
    // SDK mirrors the protocol's timestamp acceptance; execution is stricter.
    if price.updated_at > U256::from(snapshot.as_of.timestamp) {
        return Err(eyre!("oracle timestamp is in the future"));
    }
    let ttl_deadline = snapshot.as_of.timestamp.checked_add(config.ttl_seconds)
        .ok_or_else(|| eyre!("planner deadline overflows u64"))?;
    let price_expiry = price.updated_at.checked_add(U256::from(snapshot.globals.stale_window))
        .and_then(|value| value.checked_sub(U256::from(1)))
        .ok_or_else(|| eyre!("price expiry overflows/underflows"))?;
    let deadline = if price_expiry < U256::from(ttl_deadline) {
        u64::try_from(price_expiry).map_err(|_| eyre!("price deadline overflows u64"))?
    } else { ttl_deadline };
    if deadline < snapshot.as_of.timestamp { return Err(eyre!("price expired before funding plan")); }

    let debt = record.amounts.debt;
    let input = LiquidationInput {
        debt, collateral: record.amounts.collateral, requested_debt: debt,
        discount: risk.discount, max_discount: market.discount_max, price,
        min_debt_remaining: snapshot.globals.min_debt_remaining,
    };
    // Full/capped first, then cap neighbours and the debt/collateral remainder boundary. Cap equality
    // is a probe, not an assumption of drain: both ceil steps decide actual seizure.
    let mut requests = vec![if debt == u128::MAX { debt - 1 } else { debt }];
    let full_amounts = exact_amounts(&input);
    if let Ok(full) = &full_amounts {
        let recovery = checked_u128(full.max_recovery.min(U256::from(debt)))?;
        requests.push(recovery);
        if let Some(next) = recovery.checked_add(1).filter(|value| *value <= debt) { requests.push(next); }
        if recovery > 1 { requests.push(recovery - 1); }
    }
    requests.push(remainder_repay_limit(&input)?);
    for divisor in &config.partial_divisors { requests.push(debt / u128::from(*divisor)); }
    requests.push(1);

    let mut seen_requests = BTreeSet::new();
    let mut seen_amounts = BTreeSet::new();
    let mut plans = Vec::new();
    for requested in requests {
        if requested == 0 || requested == u128::MAX || requested > debt || !seen_requests.insert(requested) { continue; }
        let mut candidate = input.clone();
        candidate.requested_debt = requested;
        let amounts = match exact_amounts(&candidate) {
            Ok(amounts) => amounts,
            Err(LiquidationMathError::InsufficientLiquidationAmount { .. } | LiquidationMathError::NoDebtToLiquidate
                | LiquidationMathError::NoCollateralToSeize) => continue,
            Err(error) => return Err(error.into()),
        };
        if !seen_amounts.insert((amounts.actual_repay, amounts.seized_collateral)) { continue; }
        plans.push(FundingPlan {
            key: *key, epoch: snapshot.epoch, as_of: snapshot.as_of,
            asset: snapshot.pool.asset, collateral: snapshot.pool.collateral,
            requested_debt: requested, expected: amounts,
            min_discount_out: risk.discount, deadline,
        });
        if plans.len() == config.max_candidates { break; }
    }
    Ok(plans)
}
