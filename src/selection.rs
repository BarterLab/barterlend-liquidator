//! Bounded comparison of executable routes at one funding anchor.
use std::time::Duration;
use alloy::primitives::{Address, U256};
use color_eyre::eyre::{Result, eyre};
use futures_util::{stream::FuturesUnordered, StreamExt};
use crate::{economics::{FeePolicy, NativeCostOracle}, planner::FundingPlan,
    routes::{ExactInputRequest, ProviderRegistry, RouteQuote, RouteError},
    simulation::{prepare, SimulatedPlan, SimulationPolicy}, tx::rpc::TxRpc};

pub struct Selection {
    pub plan: SimulatedPlan,
    pub quote: RouteQuote,
    pub attempted: usize,
    pub simulated: usize,
}

/// Every selected source starts independently; a timeout cannot discard another
/// source's valid result. All candidates share funding, nonce and gas valuation.
/// This compares a rotating bounded sample, not every possible path or size.
pub async fn compare<R: TxRpc + ?Sized, C: NativeCostOracle + ?Sized>(
    registry: &ProviderRegistry, rpc: &R, native: &C, funding: &FundingPlan,
    sender: Address, executor: Address, nonce: u64, chain_id: u64,
    fees: &FeePolicy, policy: &SimulationPolicy, round: u64, budget: usize,
    route_timeout: Duration,
) -> Result<Selection> {
    let hints=registry.comparison_candidates(funding.collateral.address,funding.asset.address,round,budget)?;
    let attempted=hints.len();
    if attempted==0 || route_timeout.is_zero() { return Err(eyre!("no configured comparison routes")); }
    let request=ExactInputRequest {token_in:funding.collateral.address,token_out:funding.asset.address,
        amount_in:U256::from(funding.expected.seized_collateral),executor,at:funding.as_of};
    let mut pending=FuturesUnordered::new();
    for (index,hint) in hints.into_iter().enumerate() {
        let request=&request;
        pending.push(async move {
            let result=tokio::time::timeout(route_timeout,async {
                let quote=registry.quote(request,&hint).await?;
                let plan=prepare(rpc,native,funding,&quote,sender,executor,nonce,chain_id,fees,policy).await?;
                Ok::<_,color_eyre::eyre::Report>((plan,quote))
            }).await;
            let result=result.map_err(|_|eyre!("route comparison timed out")).and_then(|r|r);
            match &result {
                Ok((plan,quote))=>crate::diagnostics::emit_stdout(&serde_json::json!({"event":"route_candidate_validated",
                    "key":funding.key,"source":quote.route.source(),"net_profit_asset":plan.net_profit_asset(),"gas_limit":plan.envelope().gas_limit})),
                Err(error)=>crate::diagnostics::emit_stdout(&serde_json::json!({"event":"route_candidate_failed",
                    "key":funding.key,"source":hint.hint.source(),"reason":match error.downcast_ref::<RouteError>() {
                        Some(RouteError::Unavailable(_))=>"route_unavailable",
                        Some(RouteError::Stale(_))=>"expired_plan",
                        Some(RouteError::Identity(_))=>"route_identity_mismatch",
                        Some(RouteError::InvalidConfig(_))=>"route_invalid_config",
                        Some(RouteError::Rpc(_))=>"route_rpc_failed",
                        Some(RouteError::Arithmetic(_))=>"route_arithmetic",
                        None=>crate::diagnostics::safe_reason(error),
                    }})),
            }
            (index,result)
        });
    }
    let mut candidates=Vec::new();
    let mut last_error=eyre!("no profitable liquidation combination");
    while let Some((index,result))=pending.next().await {
        match result { Ok((plan,quote))=>candidates.push((index,plan,quote)),Err(error)=>last_error=error }
    }
    if candidates.is_empty() { return Err(last_error); }
    // Time spent waiting for other sources must not revive an expired winner.
    let head=rpc.head(barterlend_state::Commitment::Latest).await?;
    let anchor=rpc.block(funding.as_of.number).await?.ok_or_else(||eyre!("funding anchor missing"))?;
    if anchor.hash!=funding.as_of.hash { return Err(eyre!("funding anchor changed")); }
    let simulated=candidates.len();
    candidates.retain(|(_,plan,_)| {
        let meta=plan.metadata();
        head.number>=meta.as_of.number && head.timestamp>=meta.as_of.timestamp
            && (head.number!=meta.as_of.number || head.hash==meta.as_of.hash)
            && head.number<=meta.max_valid_block && head.timestamp<=meta.deadline
    });
    // Stable ties prefer less native exposure and then the original catalog order.
    let (_,plan,quote)=candidates.into_iter().max_by(|a,b| a.1.net_profit_asset().cmp(&b.1.net_profit_asset())
        .then_with(|| b.1.envelope().native_bound().expect("validated fee").cmp(&a.1.envelope().native_bound().expect("validated fee")))
        .then_with(||b.0.cmp(&a.0))).ok_or_else(||eyre!("all compared plans expired"))?;
    Ok(Selection {plan,quote,attempted,simulated})
}
