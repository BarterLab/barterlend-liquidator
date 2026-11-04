//! Focused execution gates: real executor request rules, converged gas guard,
//! identical signed envelope and refusal of expired/unknown funding inputs.
//! No network or state-changing RPC is used.

use std::sync::Mutex;

use alloy::{primitives::{Address, B256, Bytes, U256, keccak256}, signers::local::PrivateKeySigner,
    sol_types::{SolCall, SolValue}};
use async_trait::async_trait;
use barterlend_liquidator::{
    economics::{CostQuote, EconomicsError, EconomicsResult, FeePolicy, NativeCostOracle, NativeCostQuality},
    executor_abi::LiquidationExecutor,
    liquidation_math::LiquidationAmounts,
    planner::FundingPlan,
    routes::{QuoteValidity, RouteKind, RouteQuote, SearchQuality, encode_exact_input_path},
    simulation::{SimulationPolicy, SimulatedPlan, prepare},
    tx::{AttemptKind, JournalIdentity, Receipt, SignedAttempt, UnsignedTx, validate_attempt, rpc::TxRpc},
};
use barterlend_state::{BlockRef, ChainIdentity, Commitment, PositionKey, TokenDescriptor};
use color_eyre::eyre::{Result, eyre};

fn address(byte: u8) -> Address { Address::repeat_byte(byte) }
fn signer() -> PrivateKeySigner { "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap() }
fn chain() -> ChainIdentity { ChainIdentity { chain_id: 31337, genesis_hash: B256::repeat_byte(1) } }
fn block() -> BlockRef { BlockRef { number: 5, hash: B256::repeat_byte(5), parent_hash: B256::repeat_byte(4), timestamp: 50, commitment: Commitment::Latest } }
fn funding() -> FundingPlan {
    FundingPlan { key: PositionKey { pool: address(2), market: 1_000, borrower: address(3) }, epoch: 1,
        as_of: block(), asset: TokenDescriptor { address: address(6), decimals: 18 },
        collateral: TokenDescriptor { address: address(7), decimals: 18 }, requested_debt: 100,
        expected: LiquidationAmounts { requested_debt: 100, actual_repay: 100, seized_collateral: 200,
            remaining_debt: 10, remaining_collateral: 200, bad_debt: 0, collateral_value: U256::from(320),
            max_recovery: U256::from(256), drains_collateral: false },
        min_discount_out: U256::from(200_000_000_000_000_000u128), deadline: 70 }
}
fn quote() -> RouteQuote {
    let mut value = RouteQuote { token_in: address(7), token_out: address(6), amount_in: U256::from(200),
        amount_out: U256::from(1_000_000), minimum_amount_out: U256::from(900_000), executor: address(9), router: Some(address(12)),
        route: RouteKind::V3 { path: encode_exact_input_path(&[address(7), address(6)], &[3000]).unwrap(),
            tokens: vec![address(7), address(6)], fees: vec![3000], pools: vec![address(20)] },
        validity: QuoteValidity::at(block(), 20, 3).unwrap(), quoter_gas_estimate: U256::from(1),
        search: SearchQuality { candidates_examined: 1, quote_calls: 1, budget_exhausted: false }, fingerprint: B256::ZERO };
    value.fingerprint = value.compute_fingerprint(); value
}
fn fees() -> FeePolicy { FeePolicy { gas_margin_bps: 1000, max_gas_limit: 1_000_000,
    max_gas_price: U256::from(100), max_fee_native: U256::from(2_000_000), native_reserve: U256::from(100_000) } }
fn policy() -> SimulationPolicy { SimulationPolicy { max_iterations: 6, additional_profit_asset: U256::from(5) } }

#[derive(Clone, Copy)]
enum Returned { Valid, WrongRepay, WrongCollateral, Overspent, BelowGuard, Malformed, DirtyPadding, TrailingData }
struct RpcState {
    heads: Vec<BlockRef>, head_calls: usize, canonical_hash: B256, balance: U256,
    estimates: Vec<UnsignedTx>, simulations: Vec<UnsignedTx>, returned: Returned, divergent: bool, oscillating: bool,
    enforce_affordability: bool, route_gas: std::collections::BTreeMap<Bytes,u64>,
}
struct MockRpc(Mutex<RpcState>);
impl MockRpc {
    fn new() -> Self { Self(Mutex::new(RpcState { heads: vec![block()], head_calls: 0, canonical_hash: block().hash,
        balance: U256::from(320_000), estimates: Vec::new(), simulations: Vec::new(), returned: Returned::Valid, divergent: false, oscillating: false, enforce_affordability: false, route_gas:std::collections::BTreeMap::new() })) }
}
#[async_trait]
impl TxRpc for MockRpc {
    async fn identity(&self) -> Result<ChainIdentity> { Ok(chain()) }
    async fn code_hash(&self, _: Address, _: &BlockRef) -> Result<B256> { Ok(B256::repeat_byte(9)) }
    async fn head(&self, _: Commitment) -> Result<BlockRef> {
        let mut state = self.0.lock().unwrap();
        let index = state.head_calls.min(state.heads.len() - 1); state.head_calls += 1; Ok(state.heads[index])
    }
    async fn block(&self, number: u64) -> Result<Option<BlockRef>> { Ok((number == block().number).then(|| BlockRef { hash: self.0.lock().unwrap().canonical_hash, ..block() })) }
    async fn receipt(&self, _: B256) -> Result<Option<Receipt>> { Ok(None) }
    async fn nonce(&self, _: Address, _: bool) -> Result<u64> { Ok(7) }
    async fn balance(&self, _: Address) -> Result<U256> { Ok(self.0.lock().unwrap().balance) }
    async fn gas_price(&self) -> Result<U256> { Ok(U256::from(2)) }
    async fn estimate(&self, _: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<u64> {
        assert_eq!(*at, block());
        let request = LiquidationExecutor::executeCall::abi_decode(&tx.input)?.request;
        // The real executor rejects a zero profit guard before calling CreditPool.
        if request.minAssetProfit.is_zero() { return Err(eyre!("executor InvalidRequest: minAssetProfit is zero")); }
        let mut state = self.0.lock().unwrap();
        if state.enforce_affordability && tx.native_bound()? > state.balance {
            return Err(eyre!("estimation gas cap exceeds caller native balance"));
        }
        state.estimates.push(tx.clone());
        let estimate = if let Some(gas)=state.route_gas.get(&request.path) { *gas } else if state.divergent { 80_000 + state.estimates.len() as u64 * 1_000 }
            else if state.oscillating { if request.minAssetProfit >= U256::from(176_005) { 79_999 } else { 80_000 } }
            else if request.minAssetProfit < U256::from(176_005) { 80_000 } else { 100_000 };
        if state.enforce_affordability && estimate > tx.gas_limit {
            return Err(eyre!("gas required exceeds the supplied estimation cap"));
        }
        Ok(estimate)
    }
    async fn simulate(&self, _: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<Bytes> {
        assert_eq!(*at, block());
        let request = LiquidationExecutor::executeCall::abi_decode(&tx.input)?.request;
        let mut state = self.0.lock().unwrap(); state.simulations.push(tx.clone());
        let mut repay = 100u128; let mut collateral = 200u128; let mut spent = U256::from(199); let mut profit = request.minAssetProfit;
        match state.returned {
            Returned::Valid | Returned::DirtyPadding | Returned::TrailingData => {}, Returned::WrongRepay => repay = 99, Returned::WrongCollateral => collateral = 199,
            Returned::Overspent => spent = U256::from(201), Returned::BelowGuard => profit -= U256::from(1),
            Returned::Malformed => return Ok(Bytes::from(vec![0; 31])),
        }
        let mut bytes = (repay, collateral, spent, profit).abi_encode();
        match state.returned {
            Returned::DirtyPadding => bytes[0] = 1,
            Returned::TrailingData => bytes.extend_from_slice(&[0; 32]),
            _ => {},
        }
        Ok(bytes.into())
    }
    async fn send_raw(&self, _: &Bytes) -> Result<B256> { panic!("preparation must never send a transaction") }
}

struct CostOracle { ttl: u64, max_age: u64, unknown: bool, zero: bool, discount_after_first: u64, calls: Mutex<Vec<U256>> }
impl CostOracle { fn new() -> Self { Self { ttl: 10, max_age: 3, unknown: false, zero: false, discount_after_first: 0, calls: Mutex::new(Vec::new()) } } }
#[async_trait]
impl NativeCostOracle for CostOracle {
    async fn quote_cost(&self, asset: Address, native_bound: U256, at: &BlockRef) -> EconomicsResult<CostQuote> {
        let call_index={ let mut calls=self.calls.lock().unwrap();let index=calls.len();calls.push(native_bound);index };
        if self.unknown { return Err(EconomicsError::CostUnavailable("no verified native conversion".into())); }
        let mut quote = CostQuote { asset, native_bound, cost_asset: if self.zero { U256::ZERO } else {
            native_bound.saturating_sub(U256::from(if call_index>0 { self.discount_after_first } else { 0 }))
        },
            validity: QuoteValidity::at(*at, self.ttl, self.max_age)?,
            quality: NativeCostQuality::WrappedIdentity { wrapped_native: asset, code_hash: B256::repeat_byte(8) }, fingerprint: B256::ZERO };
        quote.fingerprint = quote.compute_fingerprint(); Ok(quote)
    }
}
async fn run(rpc: &MockRpc, cost: &CostOracle, plan: &FundingPlan, route: &RouteQuote, limits: &FeePolicy, simulation: &SimulationPolicy) -> Result<SimulatedPlan> {
    prepare(rpc, cost, plan, route, signer().address(), address(9), 7, chain().chain_id, limits, simulation).await
}

#[tokio::test]
async fn positive_probe_and_reestimate_converge_to_the_signed_limit_fee() {
    let rpc = MockRpc::new(); let cost = CostOracle::new();
    let plan = run(&rpc, &cost, &funding(), &quote(), &fees(), &policy()).await.unwrap();
    assert_eq!(plan.envelope().gas_limit, 110_000);
    assert_eq!(plan.envelope().native_bound().unwrap(), U256::from(220_000));
    assert_eq!(plan.metadata().min_asset_profit, U256::from(220_005));
    assert_eq!(plan.metadata().deadline, 59); // earliest native quote expires at 60, Solidity deadline is inclusive
    let state = rpc.0.lock().unwrap();
    assert_eq!(state.estimates.len(), 3);
    assert_eq!(state.estimates.last().unwrap(), plan.envelope());
    assert_eq!(state.simulations, vec![plan.envelope().clone()]);
    assert_eq!(*cost.calls.lock().unwrap(), vec![U256::from(176_000), U256::from(220_000), U256::from(220_000)]);
}

#[tokio::test]
async fn affordable_probe_passes_rpc_balance_checks_and_preserves_final_padded_fee() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().enforce_affordability = true;
    // The old max-limit probe costs 2,000,000 while only 220,010 is available.
    // The actual final signed limit costs exactly 220,000, leaving the required reserve.
    assert!(U256::from(fees().max_gas_limit) * U256::from(2) > rpc.0.lock().unwrap().balance);
    let plan = run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.unwrap();
    let state = rpc.0.lock().unwrap();
    assert_eq!(state.estimates[0].gas_limit, 110_000);
    assert!(state.estimates.iter().all(|tx| tx.gas_limit == 110_000),
        "higher guarded calldata must not be estimated with the previous 88,000 signed limit");
    assert!(state.estimates.iter().all(|tx| tx.native_bound().unwrap() + fees().native_reserve <= state.balance));
    assert_eq!(plan.envelope().gas_limit, 110_000); assert_eq!(plan.envelope().native_bound().unwrap(), U256::from(220_000));
    assert_eq!(plan.metadata().min_asset_profit, U256::from(220_005));
    assert_eq!(state.estimates.last().unwrap(), plan.envelope()); assert_eq!(state.simulations, vec![plan.envelope().clone()]);
}

#[tokio::test]
async fn affordable_probe_does_not_authorize_an_unfunded_padding_margin() {
    let rpc = MockRpc::new();
    { let mut state = rpc.0.lock().unwrap(); state.enforce_affordability = true; state.balance = U256::from(319_999); }
    let error = run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.unwrap_err();
    assert!(error.to_string().contains("insufficient native balance"));
    let state = rpc.0.lock().unwrap(); assert_eq!(state.estimates[0].gas_limit, 109_999);
    assert!(state.simulations.is_empty(), "one missing wei of the padded fee plus reserve must remain fatal");
}

#[tokio::test]
async fn affordable_probe_does_not_round_the_native_fee_ceiling_up() {
    let mut limits = fees(); limits.max_fee_native = U256::from(220_000);
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().enforce_affordability = true;
    let plan = run(&rpc, &CostOracle::new(), &funding(), &quote(), &limits, &policy()).await.unwrap();
    assert_eq!(plan.envelope().native_bound().unwrap(), limits.max_fee_native);
    limits.max_fee_native -= U256::from(1);
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().enforce_affordability = true;
    let error = run(&rpc, &CostOracle::new(), &funding(), &quote(), &limits, &policy()).await.unwrap_err();
    assert!(error.to_string().contains("native fee cap exceeded"));
    let state = rpc.0.lock().unwrap(); assert_eq!(state.estimates[0].gas_limit, 109_999);
    assert!(state.simulations.is_empty(), "the final ceil-padded limit still needs the complete fee ceiling");
}

#[tokio::test]
async fn exact_last_simulated_bytes_are_signed_and_tampering_fails() {
    let rpc = MockRpc::new(); let cost = CostOracle::new();
    let plan = run(&rpc, &cost, &funding(), &quote(), &fees(), &policy()).await.unwrap();
    let (envelope, meta) = plan.into_parts();
    let simulated = rpc.0.lock().unwrap().simulations.last().unwrap().clone();
    assert_eq!(envelope, simulated);
    assert_eq!(meta.simulation_fingerprint, envelope.fingerprint().unwrap());
    let raw = envelope.sign(&signer()).await.unwrap();
    let attempt = SignedAttempt { created_at_unix_ms: None, kind: AttemptKind::Liquidation(meta), envelope, hash: keccak256(&raw), raw, created_at: 50, broadcast_at: None };
    let identity = JournalIdentity { chain: chain(), signer: signer().address(), executor: address(9), executor_code_hash: B256::repeat_byte(9) };
    validate_attempt(&identity, &attempt).unwrap();
    let mut changed = attempt.clone(); changed.envelope.gas_limit += 1;
    assert!(validate_attempt(&identity, &changed).is_err());
    let mut changed = attempt; changed.envelope.input = Bytes::new();
    assert!(validate_attempt(&identity, &changed).is_err());
}

#[tokio::test]
async fn disagreement_in_actual_callback_amounts_spending_or_profit_fails_closed() {
    for returned in [Returned::WrongRepay, Returned::WrongCollateral, Returned::Overspent, Returned::BelowGuard, Returned::Malformed, Returned::DirtyPadding, Returned::TrailingData] {
        let rpc = MockRpc::new(); rpc.0.lock().unwrap().returned = returned;
        assert!(run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.is_err());
        assert_eq!(rpc.0.lock().unwrap().simulations.len(), 1);
    }
}

#[tokio::test]
async fn finite_iteration_budget_rejects_nonconverging_gas_guard() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().divergent = true;
    let limits = SimulationPolicy { max_iterations: 4, ..policy() };
    let error = run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &limits).await.unwrap_err();
    assert!(error.to_string().contains("converge"));
    assert_eq!(rpc.0.lock().unwrap().estimates.len(), 4);
    assert!(rpc.0.lock().unwrap().simulations.is_empty());
}

#[tokio::test]
async fn zero_additional_profit_accepts_exact_gas_cost_but_rejects_one_raw_unit_loss() {
    let policy = SimulationPolicy { max_iterations: 6, additional_profit_asset: U256::ZERO };
    let mut route = quote();
    // Guarded calldata converges to 88_000 gas * 2: a profit floor of 176_000.
    // Repayment 100 + gas cost 176_000 breaks even at minimum output 176_100.
    route.minimum_amount_out = U256::from(176_100); route.fingerprint = route.compute_fingerprint();
    let rpc = MockRpc::new();
    let plan = run(&rpc, &CostOracle::new(), &funding(), &route, &fees(), &policy).await.unwrap();
    assert_eq!(plan.envelope().native_bound().unwrap(), U256::from(176_000));
    assert_eq!(plan.metadata().min_asset_profit, U256::from(176_000));
    assert_eq!(route.minimum_amount_out, U256::from(funding().expected.actual_repay) + plan.metadata().min_asset_profit);
    assert_eq!(rpc.0.lock().unwrap().simulations.len(), 1);
    route.minimum_amount_out -= U256::from(1); route.fingerprint = route.compute_fingerprint();
    let rpc = MockRpc::new();
    let error = run(&rpc, &CostOracle::new(), &funding(), &route, &fees(), &policy).await.unwrap_err();
    assert_eq!(barterlend_liquidator::diagnostics::safe_reason(&error), "unprofitable_route");
    assert!(rpc.0.lock().unwrap().simulations.is_empty());
}

#[tokio::test]
async fn route_must_cover_repayment_and_final_guard_at_raw_unit_boundary() {
    let rpc = MockRpc::new(); let mut route = quote(); route.minimum_amount_out = U256::from(220_104);
    route.fingerprint = route.compute_fingerprint();
    assert!(run(&rpc, &CostOracle::new(), &funding(), &route, &fees(), &policy()).await.is_err());
    assert!(rpc.0.lock().unwrap().simulations.is_empty());
    route.minimum_amount_out += U256::from(1); route.fingerprint = route.compute_fingerprint();
    run(&MockRpc::new(), &CostOracle::new(), &funding(), &route, &fees(), &policy()).await.unwrap();
}

#[tokio::test]
async fn canonical_route_below_actual_repay_stops_before_estimation_or_signed_plan() {
    let rpc = MockRpc::new(); let cost = CostOracle::new(); let mut route = quote();
    route.amount_out = U256::from(99); route.minimum_amount_out = U256::from(99);
    route.fingerprint = route.compute_fingerprint();
    let error = run(&rpc, &cost, &funding(), &route, &fees(), &policy()).await.unwrap_err();
    assert_eq!(barterlend_liquidator::diagnostics::safe_reason(&error), "unprofitable_route");
    let state = rpc.0.lock().unwrap();
    assert!(state.estimates.is_empty()); assert!(state.simulations.is_empty());
    assert!(cost.calls.lock().unwrap().is_empty());
    // prepare only takes a sender address: Err supplies no envelope/metadata for the caller to sign.
}

#[tokio::test]
async fn early_repayment_check_uses_actual_repay_instead_of_requested_debt() {
    let rpc = MockRpc::new(); let mut plan = funding(); let mut route = quote();
    plan.requested_debt = 150; plan.expected.requested_debt = 150;
    route.amount_out = U256::from(110); route.minimum_amount_out = U256::from(110);
    route.fingerprint = route.compute_fingerprint();
    assert!(run(&rpc, &CostOracle::new(), &plan, &route, &fees(), &policy()).await.is_err());
    assert!(!rpc.0.lock().unwrap().estimates.is_empty(), "110 covers actual repay 100, while later gas guard must still fail");
    assert!(rpc.0.lock().unwrap().simulations.is_empty());
}

#[tokio::test]
async fn known_native_funding_must_include_cleanup_reserve() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().balance = U256::from(319_999);
    assert!(run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.is_err());
    assert!(rpc.0.lock().unwrap().simulations.is_empty());
    run(&MockRpc::new(), &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.unwrap();
}

#[tokio::test]
async fn unknown_or_zero_native_conversion_is_never_treated_as_free_gas() {
    for unknown in [true, false] {
        let rpc = MockRpc::new(); let cost = CostOracle { unknown, zero: !unknown, ..CostOracle::new() };
        assert!(run(&rpc, &cost, &funding(), &quote(), &fees(), &policy()).await.is_err());
        assert!(rpc.0.lock().unwrap().simulations.is_empty());
    }
}

#[tokio::test]
async fn expired_quote_or_reorged_anchor_stops_before_estimating() {
    for reorg in [false, true] {
        let rpc = MockRpc::new();
        if reorg { rpc.0.lock().unwrap().canonical_hash = B256::repeat_byte(99); }
        else { rpc.0.lock().unwrap().heads = vec![BlockRef { timestamp: 70, ..block() }]; }
        assert!(run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.is_err());
        assert!(rpc.0.lock().unwrap().estimates.is_empty());
    }
}

#[tokio::test]
async fn quote_and_funding_must_share_the_exact_target() {
    let rpc = MockRpc::new(); let mut route = quote();
    route.validity.as_of = BlockRef { number: 4, hash: B256::repeat_byte(4), parent_hash: B256::repeat_byte(3), timestamp: 49, ..block() };
    route.fingerprint = route.compute_fingerprint();
    assert!(run(&rpc, &CostOracle::new(), &funding(), &route, &fees(), &policy()).await.is_err());
    assert!(rpc.0.lock().unwrap().estimates.is_empty());
}

#[tokio::test]
async fn native_quote_age_budget_is_checked_again_after_simulation() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().heads = vec![block(), BlockRef { number: 6, hash: B256::repeat_byte(6), parent_hash: block().hash, timestamp: 51, ..block() }];
    let cost = CostOracle { max_age: 0, ..CostOracle::new() };
    assert!(run(&rpc, &cost, &funding(), &quote(), &fees(), &policy()).await.is_err());
    assert_eq!(rpc.0.lock().unwrap().simulations.len(), 1);
}

#[tokio::test]
async fn deadline_is_rechecked_after_a_slow_final_simulation() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().heads = vec![block(), BlockRef { timestamp: 60, ..block() }];
    assert!(run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &policy()).await.is_err());
    assert_eq!(rpc.0.lock().unwrap().simulations.len(), 1);
}

#[tokio::test]
async fn invalid_iteration_budget_is_rejected_without_rpc_estimation() {
    for max_iterations in [0, 9] {
        let rpc = MockRpc::new();
        assert!(run(&rpc, &CostOracle::new(), &funding(), &quote(), &fees(), &SimulationPolicy { max_iterations, ..policy() }).await.is_err());
        assert!(rpc.0.lock().unwrap().estimates.is_empty());
    }
}


#[tokio::test]
async fn oscillating_calldata_gas_estimates_keep_conservative_guard_and_converge() {
    let rpc=MockRpc::new();rpc.0.lock().unwrap().oscillating=true;let cost=CostOracle::new();
    // 80_000 -> padded 88_000 -> guard 176_005; that calldata estimates 79_999,
    // which would lower the guard to 176_003 and cause 80_000 again forever.
    let plan=run(&rpc,&cost,&funding(),&quote(),&fees(),&policy()).await
        .expect("a one-gas calldata oscillation must converge by retaining the conservative bound");
    assert_eq!(plan.envelope().gas_limit,88_000);
    assert_eq!(plan.envelope().native_bound().unwrap(),U256::from(176_000));
    assert_eq!(plan.metadata().min_asset_profit,U256::from(176_005));
    let state=rpc.0.lock().unwrap();assert_eq!(state.estimates.len(),2);
    assert_eq!(state.simulations,vec![plan.envelope().clone()]);
    assert_eq!(*cost.calls.lock().unwrap(),vec![U256::from(176_000),U256::from(176_000)],
        "native conversion must use the retained signed bound, not the smaller current estimate");
}


#[tokio::test]
async fn cheaper_native_conversion_does_not_lower_the_existing_calldata_guard() {
    let rpc=MockRpc::new();let cost=CostOracle { discount_after_first:50_000,..CostOracle::new() };
    let plan=run(&rpc,&cost,&funding(),&quote(),&fees(),&policy()).await.unwrap();
    assert_eq!(plan.envelope().gas_limit,110_000,"a smaller later native quote cannot undo an already validated gas bound");
    assert_eq!(plan.metadata().min_asset_profit,U256::from(176_005));
    let state=rpc.0.lock().unwrap();let guards:Vec<_>=state.estimates.iter()
        .map(|tx|LiquidationExecutor::executeCall::abi_decode(&tx.input).unwrap().request.minAssetProfit).collect();
    assert_eq!(guards,vec![U256::from(1),U256::from(176_005),U256::from(176_005)]);
    assert_eq!(state.simulations,vec![plan.envelope().clone()]);
    assert_eq!(*cost.calls.lock().unwrap(),vec![U256::from(176_000),U256::from(220_000),U256::from(220_000)]);
}

#[tokio::test]
async fn oscillation_fix_does_not_relax_fee_caps_or_final_profit_verification() {
    for too_small_fee_cap in [true,false] {
        let rpc=MockRpc::new();{ let mut state=rpc.0.lock().unwrap();state.oscillating=true;
            if !too_small_fee_cap { state.returned=Returned::BelowGuard; }
        }
        let mut limits=fees();if too_small_fee_cap { limits.max_fee_native=U256::from(175_999); }
        assert!(run(&rpc,&CostOracle::new(),&funding(),&quote(),&limits,&policy()).await.is_err());
        assert_eq!(rpc.0.lock().unwrap().simulations.len(),if too_small_fee_cap { 0 } else { 1 });
    }
}


// Provider comparison uses the real financial preparation against the same
// deterministic execution boundary as the gas/nonce tests above.
struct Offer {
    value: RouteQuote,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    behavior: u8,
}
#[async_trait]
impl barterlend_liquidator::routes::LiquidityProvider for Offer {
    fn name(&self)->&'static str { "fixture" }
    fn capabilities(&self)->barterlend_liquidator::routes::registry::Capabilities {
        barterlend_liquidator::routes::registry::Capabilities {exact_input:true,exact_output:false}
    }
    fn candidates(&self,_:Address,_:Address)->barterlend_liquidator::routes::RouteResult<Vec<RouteKind>> {Ok(vec![self.value.route.clone()])}
    async fn quote(&self,request:&barterlend_liquidator::routes::ExactInputRequest,_:&RouteKind)->barterlend_liquidator::routes::RouteResult<RouteQuote> {
        self.calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
        assert_eq!(request.at,self.value.validity.as_of);
        assert_eq!(request.amount_in,self.value.amount_in);
        match self.behavior {
            1=>return std::future::pending().await,
            2=>return Err(barterlend_liquidator::routes::RouteError::Unavailable("fixture failure".into())),
            _=>{},
        }
        Ok(self.value.clone())
    }
}
fn offer(mut q:RouteQuote,behavior:u8)->(std::sync::Arc<Offer>,std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    q.fingerprint=q.compute_fingerprint();
    let calls=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (std::sync::Arc::new(Offer{value:q,calls:calls.clone(),behavior}),calls)
}
async fn compare(rpc:&MockRpc, offers:Vec<std::sync::Arc<Offer>>) -> Result<barterlend_liquidator::selection::Selection> {
    let registry=barterlend_liquidator::routes::ProviderRegistry::new(offers.into_iter().map(|p|p as std::sync::Arc<dyn barterlend_liquidator::routes::LiquidityProvider>).collect()).unwrap();
    barterlend_liquidator::selection::compare(&registry,rpc,&CostOracle::new(),&funding(),signer().address(),address(9),7,31337,
        &fees(),&policy(),0,8,std::time::Duration::from_millis(100)).await
}
#[tokio::test]
async fn comparison_queries_later_provider_and_selects_higher_net() {
    let first=quote();let mut second=first.clone();second.minimum_amount_out+=U256::from(1000);second.amount_out+=U256::from(1000);
    let (a,ac)=offer(first,0);let (b,bc)=offer(second.clone(),0);
    let result=compare(&MockRpc::new(),vec![a,b]).await.unwrap();
    assert_eq!(ac.load(std::sync::atomic::Ordering::SeqCst),1);
    assert_eq!(bc.load(std::sync::atomic::Ordering::SeqCst),1);
    assert_eq!(result.quote.minimum_amount_out,second.minimum_amount_out);
    assert_eq!(result.attempted,2);assert_eq!(result.simulated,2);
    assert_eq!(result.plan.net_profit_asset(),U256::from(900_900)-result.plan.metadata().min_asset_profit+policy().additional_profit_asset);
}
#[tokio::test]
async fn higher_output_does_not_win_when_its_signed_gas_cost_is_higher() {
    let cheap=quote();let mut expensive=cheap.clone();
    expensive.route=RouteKind::V3 {path:encode_exact_input_path(&[address(7),address(6)],&[500]).unwrap(),tokens:vec![address(7),address(6)],fees:vec![500],pools:vec![address(21)]};
    expensive.minimum_amount_out+=U256::from(20_000);expensive.amount_out+=U256::from(20_000);
    let rpc=MockRpc::new();
    rpc.0.lock().unwrap().route_gas.insert(cheap.route.exact_input_path(),80_000);
    rpc.0.lock().unwrap().route_gas.insert(expensive.route.exact_input_path(),100_000);
    let (a,_)=offer(expensive,0);let (b,_)=offer(cheap.clone(),0);
    let result=compare(&rpc,vec![a,b]).await.unwrap();
    assert_eq!(result.simulated,2);assert_eq!(result.quote.route,cheap.route);
    assert_eq!(result.plan.envelope().gas_limit,88_000);
}
#[tokio::test]
async fn failed_or_timed_out_provider_preserves_profitable_alternative() {
    for behavior in [1,2] {
        let (a,ac)=offer(quote(),behavior);let (b,bc)=offer(quote(),0);
        let result=compare(&MockRpc::new(),vec![a,b]).await.unwrap();
        assert_eq!(result.simulated,1);
        assert_eq!(ac.load(std::sync::atomic::Ordering::SeqCst),1);
        assert_eq!(bc.load(std::sync::atomic::Ordering::SeqCst),1);
    }
}
#[tokio::test]
async fn comparison_rejects_winner_expired_while_other_work_completed() {
    let rpc=MockRpc::new();
    rpc.0.lock().unwrap().heads=vec![block(),block(),BlockRef{number:9,timestamp:54,..block()}];
    let (a,_)=offer(quote(),0);
    let result=compare(&rpc,vec![a]).await;
    assert!(result.err().unwrap().to_string().contains("all compared plans expired"));
    assert_eq!(rpc.0.lock().unwrap().simulations.len(),1);
}
#[tokio::test]
async fn zero_net_route_is_eligible_in_comparison() {
    let rpc=MockRpc::new();let mut q=quote();
    rpc.0.lock().unwrap().route_gas.insert(q.route.exact_input_path(),80_000);
    q.minimum_amount_out=U256::from(176_100);q.amount_out=q.minimum_amount_out;
    let (a,_)=offer(q,0);
    let registry=barterlend_liquidator::routes::ProviderRegistry::new(vec![a]).unwrap();
    let selected=barterlend_liquidator::selection::compare(&registry,&rpc,&CostOracle::new(),&funding(),signer().address(),address(9),7,31337,
        &fees(),&SimulationPolicy{additional_profit_asset:U256::ZERO,..policy()},0,8,std::time::Duration::from_millis(100)).await.unwrap();
    assert_eq!(selected.plan.net_profit_asset(),U256::ZERO);
}
