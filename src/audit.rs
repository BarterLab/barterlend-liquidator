use alloy::{primitives::U256, sol_types::SolEvent};
use color_eyre::eyre::{Result, eyre};
use crate::{executor_abi::LiquidationExecutor, tx::*};

pub fn audit_outcome(identity: &JournalIdentity, attempt: &SignedAttempt, receipt: &Receipt, attempts: Vec<SignedAttempt>, native_cost_asset: Option<U256>) -> Result<Outcome> {
    validate_attempt(identity, attempt)?;
    validate_attempt_history(identity, &attempts)?;
    if !attempts.iter().any(|recorded| recorded == attempt) { return Err(eyre!("winning attempt is not recorded")); }
    if receipt.hash != attempt.hash || receipt.block_hash.is_zero() || receipt.gas_used < 21_000
        || receipt.gas_used > attempt.envelope.gas_limit || receipt.effective_gas_price != attempt.envelope.gas_price
        || !receipt.succeeded && !receipt.logs.is_empty()
    { return Err(eyre!("receipt fee/hash disagrees with signed legacy envelope")); }
    let native_charge = U256::from(attempt.envelope.gas_limit).checked_mul(receipt.effective_gas_price).ok_or_else(|| eyre!("native charge overflow"))?;
    let mut asset_gain = None;
    let kind = if !receipt.succeeded { OutcomeKind::Reverted } else { match &attempt.kind {
        AttemptKind::Cancel => OutcomeKind::Cancelled,
        AttemptKind::Liquidation(meta) => {
            let relevant: Vec<_> = receipt.logs.iter().filter(|l| l.address == identity.executor && l.topics.first() == Some(&LiquidationExecutor::LiquidationExecuted::SIGNATURE_HASH)).collect();
            let event = if relevant.len() == 1 {
                let log = relevant[0];
                LiquidationExecutor::LiquidationExecuted::decode_raw_log_validate(log.topics.iter().copied(), &log.data).ok()
                    .filter(|event| {
                        let encoded = event.encode_log_data();
                        encoded.topics() == log.topics.as_slice() && encoded.data == log.data
                    })
            } else { None };
            match event {
                Some(event) if event.pool == meta.key.pool && event.borrower == meta.key.borrower && event.market == meta.key.market
                    && event.planFingerprint == meta.plan_fingerprint && event.actualRepay > 0 && U256::from(event.actualRepay) <= meta.requested_debt
                    && event.collateralSeized > 0 && event.collateralSpent <= U256::from(event.collateralSeized) && event.assetProfit >= meta.min_asset_profit
                    && meta.min_swap_output.is_none_or(|minimum| event.assetProfit.checked_add(U256::from(event.actualRepay)).is_some_and(|output| output >= minimum)) => {
                        asset_gain = Some(event.assetProfit); OutcomeKind::Liquidated
                    }
                _ => OutcomeKind::InvalidSuccessEvent,
            }
        }
    }};
    // Unknown FX remains explicit. Never assume planned conversion is a realised charge.
    let net_asset = match (asset_gain, native_cost_asset) { (Some(gain), Some(cost)) => gain.checked_sub(cost), _ => None };
    Ok(Outcome { identity: identity.clone(), attempt: attempt.clone(), receipt: receipt.clone(), kind, native_charge, asset_gain, net_asset, attempts })
}
