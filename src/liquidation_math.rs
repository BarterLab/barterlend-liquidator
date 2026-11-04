//! Exact CreditPool v5 rules: raw units, two round-ups, recovery cap and remaining collateral.
//! Eligibility, oracle freshness and deadlines are checked before this pure amount model.

use std::fmt;

use alloy::primitives::{U256, U512};
use barterlend_state::Price;
use serde::{Deserialize, Serialize};

pub const UNIT: u128 = 1_000_000_000_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationInput {
    pub debt: u128,
    pub collateral: u128,
    /// Exact protocol input. MAX is supported here for differential verification;
    /// the planner deliberately never emits the protocol's expandable sentinel.
    pub requested_debt: u128,
    pub discount: U256,
    /// Effective maximum discount of this market, used to value the remainder.
    pub max_discount: U256,
    pub price: Price,
    pub min_debt_remaining: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationAmounts {
    /// Resolved request: MAX sentinel, if supplied to the math API, resolves to D.
    pub requested_debt: u128,
    pub actual_repay: u128,
    pub seized_collateral: u128,
    pub remaining_debt: u128,
    pub remaining_collateral: u128,
    pub bad_debt: u128,
    pub collateral_value: U256,
    pub max_recovery: U256,
    pub drains_collateral: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiquidationMathError {
    NoDebt,
    AmountOutOfRange { requested: u128, debt: u128 },
    NoCollateralToSeize,
    NoDebtToLiquidate,
    InvalidDiscount,
    InvalidPrice,
    ArithmeticOverflow,
    InsufficientLiquidationAmount {
        remaining_debt: u128,
        remaining_collateral_value: U256,
        minimum_remaining: u128,
    },
}

impl fmt::Display for LiquidationMathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "liquidation v5 amount model: {self:?}")
    }
}

impl std::error::Error for LiquidationMathError {}

/// Solidity Math.mulDiv semantics with a 512-bit intermediate and a U256 result.
pub fn mul_div_floor(a: U256, b: U256, denominator: U256) -> Result<U256, LiquidationMathError> {
    mul_div(a, b, denominator, false)
}

pub fn mul_div_ceil(a: U256, b: U256, denominator: U256) -> Result<U256, LiquidationMathError> {
    mul_div(a, b, denominator, true)
}

fn mul_div(a: U256, b: U256, denominator: U256, ceil: bool) -> Result<U256, LiquidationMathError> {
    if denominator.is_zero() { return Err(LiquidationMathError::InvalidPrice); }
    let product = U512::from(a) * U512::from(b);
    let denominator = U512::from(denominator);
    let mut result = product / denominator;
    if ceil && product % denominator != U512::ZERO {
        result = result.checked_add(U512::from(1)).ok_or(LiquidationMathError::ArithmeticOverflow)?;
    }
    if result > U512::from(U256::MAX) { return Err(LiquidationMathError::ArithmeticOverflow); }
    let limbs = result.as_limbs();
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

pub fn checked_u128(value: U256) -> Result<u128, LiquidationMathError> {
    if value > U256::from(u128::MAX) { return Err(LiquidationMathError::ArithmeticOverflow); }
    let limbs = value.as_limbs();
    Ok(u128::from(limbs[0]) | (u128::from(limbs[1]) << 64))
}

fn validate_price_and_discounts(input: &LiquidationInput) -> Result<(), LiquidationMathError> {
    if input.max_discount >= U256::from(UNIT) || input.discount > input.max_discount {
        return Err(LiquidationMathError::InvalidDiscount);
    }
    if input.price.numerator.is_zero() || input.price.denominator.is_zero()
        || input.price.numerator / input.price.denominator > U256::from(u128::MAX)
    {
        return Err(LiquidationMathError::InvalidPrice);
    }
    Ok(())
}

/// Upper request boundary imposed by the two minimum-remainder conditions.
/// The caller still checks the result with `exact_amounts`: full repayment and
/// collateral exhaustion are exempt from these conditions on-chain.
pub fn remainder_repay_limit(input: &LiquidationInput) -> Result<u128, LiquidationMathError> {
    validate_price_and_discounts(input)?;
    let Some(debt_limit) = input.debt.checked_sub(input.min_debt_remaining) else { return Ok(0); };
    let unit = U256::from(UNIT);
    let collateral_value = mul_div_floor(U256::from(input.collateral), input.price.numerator, input.price.denominator)?;
    if mul_div_floor(collateral_value, unit - input.max_discount, unit)? < U256::from(input.min_debt_remaining) {
        return Ok(0);
    }
    // Invert both floor conversions on the residual collateral, then both ceil
    // conversions on seized collateral. Combining either pair changes raw units.
    let minimum_value = mul_div_ceil(U256::from(input.min_debt_remaining), unit, unit - input.max_discount)?;
    let minimum_collateral = mul_div_ceil(minimum_value, input.price.denominator, input.price.numerator)?;
    if minimum_collateral > U256::from(input.collateral) { return Ok(0); }
    let maximum_seizure = U256::from(input.collateral) - minimum_collateral;
    let maximum_undiscounted = mul_div_floor(maximum_seizure, unit - input.discount, unit)?;
    let collateral_limit = mul_div_floor(maximum_undiscounted, input.price.numerator, input.price.denominator)?;
    checked_u128(collateral_limit.min(U256::from(debt_limit)))
}

/// Exact successful callback amounts and post-liquidation borrower balances.
/// This repeats `_collateralToSeize` and its remainder/socialization rules;
/// there is no percentage close factor and no additive liquidation bonus.
pub fn exact_amounts(input: &LiquidationInput) -> Result<LiquidationAmounts, LiquidationMathError> {
    if input.debt == 0 { return Err(LiquidationMathError::NoDebt); }
    let requested = if input.requested_debt == u128::MAX { input.debt } else { input.requested_debt };
    if requested == 0 || requested > input.debt {
        return Err(LiquidationMathError::AmountOutOfRange { requested, debt: input.debt });
    }
    if input.collateral == 0 { return Err(LiquidationMathError::NoCollateralToSeize); }
    let unit = U256::from(UNIT);
    validate_price_and_discounts(input)?;
    let remaining_fraction = unit - input.discount;
    let collateral_value = mul_div_floor(U256::from(input.collateral), input.price.numerator, input.price.denominator)?;
    let max_recovery = mul_div_floor(collateral_value, remaining_fraction, unit)?;
    if max_recovery.is_zero() { return Err(LiquidationMathError::NoDebtToLiquidate); }

    let mut seizure = U256::from(input.collateral);
    if U256::from(requested) <= collateral_value {
        let undiscounted = mul_div_ceil(U256::from(requested), input.price.denominator, input.price.numerator)?;
        seizure = mul_div_ceil(undiscounted, unit, remaining_fraction)?;
    }
    let drains_collateral = seizure >= U256::from(input.collateral);
    let (seized_collateral, actual_repay) = if drains_collateral {
        (input.collateral, checked_u128(U256::from(requested).min(max_recovery))?)
    } else {
        (checked_u128(seizure)?, requested)
    };
    if seized_collateral == 0 { return Err(LiquidationMathError::NoCollateralToSeize); }
    let debt_after_repay = input.debt - actual_repay;
    if actual_repay != input.debt && !drains_collateral {
        let remaining_collateral_value = mul_div_floor(
            U256::from(input.collateral - seized_collateral), input.price.numerator, input.price.denominator,
        )?;
        let remaining_recovery = mul_div_floor(remaining_collateral_value, unit - input.max_discount, unit)?;
        if U256::from(debt_after_repay).min(remaining_recovery) < U256::from(input.min_debt_remaining) {
            return Err(LiquidationMathError::InsufficientLiquidationAmount {
                remaining_debt: debt_after_repay, remaining_collateral_value,
                minimum_remaining: input.min_debt_remaining,
            });
        }
    }
    let bad_debt = if drains_collateral { debt_after_repay } else { 0 };
    Ok(LiquidationAmounts {
        requested_debt: requested,
        actual_repay,
        seized_collateral,
        remaining_debt: debt_after_repay - bad_debt,
        remaining_collateral: input.collateral - seized_collateral,
        bad_debt,
        collateral_value,
        max_recovery,
        drains_collateral,
    })
}
