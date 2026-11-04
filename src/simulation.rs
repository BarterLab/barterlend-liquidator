use alloy::{primitives::{Address, U256, keccak256}, sol_types::SolCall};
use color_eyre::eyre::{Result, eyre};
use crate::{executor_abi::{LiquidationExecutor, Request}, planner::FundingPlan, routes::RouteQuote,
    economics::{FeePolicy, LegacyFeePlan, NativeCostOracle, build_expected_economics}, tx::{UnsignedTx, PlanMeta, rpc::TxRpc}};

#[derive(Clone, Debug)]
pub struct SimulationPolicy { pub max_iterations: usize, pub additional_profit_asset: U256 }

/// Fields are private: consumers cannot change calldata/gas after the last simulation.
#[derive(Clone, Debug)]
pub struct SimulatedPlan { envelope: UnsignedTx, meta: PlanMeta, net_profit_asset: U256 }

impl SimulatedPlan {
    pub fn envelope(&self) -> &UnsignedTx { &self.envelope }
    pub fn metadata(&self) -> &PlanMeta { &self.meta }
    /// Conservative debt-token surplus after repayment and the signed gas guard.
    pub fn net_profit_asset(&self) -> U256 { self.net_profit_asset }
    pub fn into_parts(self) -> (UnsignedTx, PlanMeta) { (self.envelope, self.meta) }
}

pub async fn prepare<R: TxRpc + ?Sized, C: NativeCostOracle + ?Sized>(
    rpc: &R, native_cost: &C, funding: &FundingPlan, quote: &RouteQuote,
    sender: Address, executor: Address, nonce: u64, chain_id: u64,
    fees: &FeePolicy, policy: &SimulationPolicy,
) -> Result<SimulatedPlan> {
    if policy.max_iterations == 0 || policy.max_iterations > 8 { return Err(eyre!("bounded simulation requires 1..=8 iterations")); }
    quote.validate(funding.collateral.address, funding.asset.address, U256::from(funding.expected.seized_collateral), executor, &funding.as_of)?;
    if quote.validity.as_of != funding.as_of { return Err(eyre!("route and funding must use the same canonical anchor")); }
    if rpc.identity().await?.chain_id != chain_id { return Err(eyre!("simulation chain differs from explicit identity")); }
    // All quote/state reads are on exactly this canonical block. Caller obtains fresh latest FundingPlan.
    let latest = rpc.head(barterlend_state::Commitment::Latest).await?;
    if latest.number < funding.as_of.number || latest.timestamp < funding.as_of.timestamp || latest.timestamp > funding.deadline { return Err(eyre!("fresh funding state expired")); }
    quote.validity.validate(&latest)?;
    let anchor = rpc.block(funding.as_of.number).await?.ok_or_else(|| eyre!("funding anchor missing"))?;
    if anchor.hash != funding.as_of.hash { return Err(eyre!("funding anchor changed")); }
    // A canonical quote which cannot repay even before gas cannot reach a safe executor callback.
    // Reject here so the diagnostic remains economic rather than a downstream estimation revert.
    if quote.minimum_amount_out < U256::from(funding.expected.actual_repay) {
        return Err(eyre!("route output does not cover repayment plus signed gas bound"));
    }
    let gas_price = rpc.gas_price().await?;
    let balance = rpc.balance(sender).await?;
    let probe_limit = fees.estimation_probe_limit(gas_price, balance)?;
    let plan_fingerprint = keccak256(serde_json::to_vec(&(funding, quote.fingerprint, sender, executor, nonce, chain_id))?);
    let mut request = Request { pool: funding.key.pool, market: funding.key.market, borrower: funding.key.borrower,
        requestedDebt: funding.requested_debt, minDiscountOut: funding.min_discount_out,
        deadline: U256::from(funding.deadline.min(quote.validity.expires_at.checked_sub(1).ok_or_else(|| eyre!("invalid route expiry"))?)),
        path: quote.route.exact_input_path(), minAssetProfit: U256::from(1), minSwapOutput: quote.minimum_amount_out, planFingerprint: plan_fingerprint };
    let mut tx = UnsignedTx { chain_id, nonce, to: executor, input: LiquidationExecutor::executeCall { request: request.clone() }.abi_encode().into(), gas_limit: probe_limit, gas_price };
    let mut converged = false;
    let mut final_cost_validity = None;
    let mut retained_fee: Option<LegacyFeePlan> = None;
    for _ in 0..policy.max_iterations {
        // A new encoded guard may require more gas than the previous padded result.
        // Estimation retains the affordable cap; only the final exact simulation uses
        // the selected signed limit. Neither operation may spend the cleanup reserve.
        let probe = UnsignedTx { gas_limit: probe_limit, ..tx.clone() };
        let estimate = rpc.estimate(sender, &probe, &funding.as_of).await?;
        let estimated_fee = fees.plan_legacy(estimate, gas_price, balance)?;
        // Encoding the profit guard changes calldata gas and can make adjacent
        // estimates oscillate. Keep the greatest validated bound encountered;
        // every new estimate still passes all gas, fee and balance policy checks.
        let fee = match &retained_fee {
            Some(previous) if previous.signed_gas_limit > estimated_fee.signed_gas_limit => previous.clone(),
            _ => estimated_fee,
        };
        retained_fee = Some(fee.clone());
        let cost = native_cost.quote_cost(funding.asset.address, fee.fee_bound_native, &funding.as_of).await?;
        cost.validity.validate(&funding.as_of)?;
        final_cost_validity = Some(cost.validity.clone());
        let deadline = funding.deadline.min(quote.validity.expires_at.saturating_sub(1)).min(cost.validity.expires_at.saturating_sub(1));
        if deadline < funding.as_of.timestamp { return Err(eyre!("native cost expired")); }
        let economics = build_expected_economics(funding.asset.address, fee, cost, policy.additional_profit_asset, &funding.as_of)?;
        let min_asset_profit = request.minAssetProfit.max(economics.min_asset_profit);
        let required = U256::from(funding.expected.actual_repay).checked_add(min_asset_profit).ok_or_else(|| eyre!("minimum output overflow"))?;
        if quote.minimum_amount_out < required { return Err(eyre!("route output does not cover repayment plus signed gas bound")); }
        request.deadline = request.deadline.min(U256::from(deadline)); request.minAssetProfit = min_asset_profit;
        let input = LiquidationExecutor::executeCall { request: request.clone() }.abi_encode().into();
        let next = UnsignedTx { input, gas_limit: economics.signed_gas_limit, ..tx.clone() };
        if next == tx { converged = true; break; }
        tx = next;
    }
    if !converged { return Err(eyre!("gas/profit guard did not converge within iteration budget")); }
    let returned = rpc.simulate(sender, &tx, &funding.as_of).await?;
    let result = crate::abi_validation::decode_return::<LiquidationExecutor::executeCall>(&returned)?;
    if result.actualRepay == 0 || result.actualRepay != funding.expected.actual_repay || result.collateralSeized != funding.expected.seized_collateral
        || result.collateralSpent > U256::from(result.collateralSeized) || result.assetProfit < request.minAssetProfit
    { return Err(eyre!("actual simulation amounts disagree with protocol funding model/guard")); }
    let latest = rpc.head(barterlend_state::Commitment::Latest).await?;
    quote.validity.validate(&latest)?;
    final_cost_validity.as_ref().ok_or_else(|| eyre!("native cost validation missing"))?.validate(&latest)?;
    if U256::from(latest.timestamp) > request.deadline { return Err(eyre!("plan expired during final simulation")); }
    let native_validity = final_cost_validity.ok_or_else(|| eyre!("native cost validation missing"))?;
    let max_valid_block = quote.validity.as_of.number.checked_add(quote.validity.max_age_blocks).and_then(|route_bound| native_validity.as_of.number.checked_add(native_validity.max_age_blocks).map(|cost_bound| route_bound.min(cost_bound))).ok_or_else(|| eyre!("quote block-age bound overflow"))?;
    let meta = PlanMeta { key: funding.key, epoch: funding.epoch, as_of: funding.as_of, max_valid_block,
        deadline: u64::try_from(request.deadline).map_err(|_| eyre!("deadline overflow"))?, requested_debt: U256::from(funding.requested_debt),
        min_discount: funding.min_discount_out, min_asset_profit: request.minAssetProfit, min_swap_output: Some(request.minSwapOutput),
        plan_fingerprint, simulation_fingerprint: tx.fingerprint()?, paid_state_fingerprint: funding.paid_state_fingerprint()?, asset: funding.asset.address };
    let identity = crate::tx::JournalIdentity { chain: rpc.identity().await?, signer: sender, executor, executor_code_hash: rpc.code_hash(executor, &funding.as_of).await? };
    crate::tx::validate_unsigned(&identity, &tx, &crate::tx::AttemptKind::Liquidation(meta.clone()))?;
    let gas_guard=request.minAssetProfit.checked_sub(policy.additional_profit_asset).ok_or_else(||eyre!("profit guard below policy"))?;
    let net_profit_asset=quote.minimum_amount_out.checked_sub(U256::from(funding.expected.actual_repay))
        .and_then(|profit|profit.checked_sub(gas_guard)).ok_or_else(||eyre!("negative net route outcome"))?;
    Ok(SimulatedPlan { envelope: tx, meta, net_profit_asset })
}
