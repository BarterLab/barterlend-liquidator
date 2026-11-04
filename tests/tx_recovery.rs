//! In-process transaction tests use real legacy signatures and an in-memory local chain.
//! No network endpoint or externally usable deployment is involved.

use std::{collections::BTreeMap, sync::{Arc, Mutex}, time::{SystemTime, UNIX_EPOCH}};

use alloy::{primitives::{Address, B256, Bytes, U256, keccak256}, signers::local::PrivateKeySigner,
    sol_types::{SolCall, SolEvent}};
use async_trait::async_trait;
use barterlend_state::{BlockRef, ChainIdentity, Commitment, PositionKey};
use barterlend_liquidator::{executor_abi::{LiquidationExecutor, Request}, tx::{*, rpc::TxRpc}};
use barterlend_liquidator::{app::{ensure_operator_gate, ensure_send_budget, startup_reconcile}, audit::audit_outcome};
use barterlend_liquidator::{economics::{CostQuote, NativeCostQuality}, routes::QuoteValidity};
use color_eyre::eyre::{Result, eyre};

fn signer() -> PrivateKeySigner {
    // Public, deliberately unfunded test key; never connected to an external RPC.
    "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap()
}

fn identity() -> JournalIdentity {
    JournalIdentity { chain: ChainIdentity { chain_id: 31337, genesis_hash: B256::repeat_byte(1) },
        signer: signer().address(), executor: Address::repeat_byte(4), executor_code_hash: B256::repeat_byte(6) }
}

fn block(number: u64) -> BlockRef {
    BlockRef { number, hash: B256::repeat_byte((number + 1) as u8), parent_hash: B256::repeat_byte(number as u8),
        timestamp: 100 + number, commitment: Commitment::Latest }
}

fn liquidation_unsigned(deadline: u64, gas_price: u64) -> (UnsignedTx, AttemptKind) {
    let key = PositionKey { pool: Address::repeat_byte(2), market: 1_000, borrower: Address::repeat_byte(3) };
    let fingerprint = B256::repeat_byte(9);
    let request = Request { pool: key.pool, market: key.market, borrower: key.borrower,
        requestedDebt: 100, minDiscountOut: U256::from(1), deadline: U256::from(deadline),
        path: Bytes::new(), minAssetProfit: U256::from(1), minSwapOutput: U256::from(1), planFingerprint: fingerprint };
    let envelope = UnsignedTx { chain_id: 31337, nonce: 7, to: identity().executor,
        input: LiquidationExecutor::executeCall { request }.abi_encode().into(), gas_limit: 100_000,
        gas_price: U256::from(gas_price) };
    let meta = PlanMeta { key, epoch: 1, as_of: block(1), deadline, max_valid_block: 1_000, requested_debt: U256::from(100),
        min_discount: U256::from(1), min_asset_profit: U256::from(1), min_swap_output: Some(U256::from(1)), plan_fingerprint: fingerprint,
        simulation_fingerprint: envelope.fingerprint().unwrap(), paid_state_fingerprint: B256::repeat_byte(10), asset: Address::repeat_byte(5) };
    (envelope, AttemptKind::Liquidation(meta))
}

async fn signed(envelope: UnsignedTx, kind: AttemptKind) -> SignedAttempt {
    let raw = envelope.sign(&signer()).await.unwrap();
    SignedAttempt { hash: keccak256(&raw), raw, envelope, kind, created_at: 101, broadcast_at: None, created_at_unix_ms: None }
}

async fn liquidation_attempt(deadline: u64) -> SignedAttempt {
    let (envelope, kind) = liquidation_unsigned(deadline, 1);
    signed(envelope, kind).await
}

async fn liquidation_with_profit(deadline: u64, profit: u64) -> SignedAttempt {
    let (mut envelope, mut kind) = liquidation_unsigned(deadline, 1);
    let mut call = LiquidationExecutor::executeCall::abi_decode(&envelope.input).unwrap();
    call.request.minAssetProfit = U256::from(profit);
    envelope.input = call.abi_encode().into();
    if let AttemptKind::Liquidation(ref mut meta) = kind {
        meta.min_asset_profit = U256::from(profit);
        meta.simulation_fingerprint = envelope.fingerprint().unwrap();
    }
    signed(envelope, kind).await
}

fn native_cost(at: BlockRef, expires_at: u64, max_age_blocks: u64, cost_asset: u64) -> CostQuote {
    let mut cost = CostQuote { asset: Address::repeat_byte(5), native_bound: U256::from(100_000),
        cost_asset: U256::from(cost_asset), validity: QuoteValidity { as_of: at, expires_at, max_age_blocks },
        quality: NativeCostQuality::V3ExactOutput { quoter: Address::repeat_byte(7), route_fingerprint: B256::repeat_byte(8) },
        fingerprint: B256::ZERO };
    cost.fingerprint = cost.compute_fingerprint();
    cost
}

async fn cancellation_attempt() -> SignedAttempt {
    signed(UnsignedTx { chain_id: 31337, nonce: 7, to: signer().address(), input: Bytes::new(),
        gas_limit: 21_000, gas_price: U256::from(2) }, AttemptKind::Cancel).await
}

#[derive(Clone, Copy, Debug)]
enum FinalityFault { InclusionHash, InclusionParent, InclusionTime, MissingInclusion, MissingFinalized, Receipt, ChainIdentity, ExecutorCode, FinalizedRegression, FinalizedHash, FinalizedAdvance }

struct Chain {
    identity: ChainIdentity,
    executor_code_hash: B256,
    blocks: BTreeMap<u64, BlockRef>,
    latest: u64,
    finalized: u64,
    nonce: u64,
    pending_nonce: u64,
    receipts: BTreeMap<B256, Receipt>,
    sent: Vec<Bytes>,
    fail_send: bool,
    fail_receipts: bool,
    invalid_final_anchor: bool,
    simulations: usize,
    advance_head_on_simulate: Option<u64>,
    block_calls: Vec<u64>,
    receipt_calls: BTreeMap<B256, usize>,
    identity_calls: usize,
    code_calls: usize,
    finalized_calls: usize,
    recheck_fault: Option<FinalityFault>,
}

struct MockRpc(Mutex<Chain>);

impl MockRpc {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Chain { identity: identity().chain, executor_code_hash: identity().executor_code_hash,
            blocks: (0..=20).map(|number| (number, block(number))).collect(), latest: 10, finalized: 3,
            nonce: 7, pending_nonce: 8, receipts: BTreeMap::new(), sent: Vec::new(), fail_send: false,
            fail_receipts: false, invalid_final_anchor: false, simulations: 0, advance_head_on_simulate: None,
            block_calls: Vec::new(), receipt_calls: BTreeMap::new(), identity_calls: 0, code_calls: 0,
            finalized_calls: 0, recheck_fault: None })))
    }
}

#[async_trait]
impl TxRpc for MockRpc {
    async fn identity(&self) -> Result<ChainIdentity> {
        let mut data = self.0.lock().unwrap(); data.identity_calls += 1;
        let mut identity = data.identity;
        if data.identity_calls > 1 && matches!(data.recheck_fault, Some(FinalityFault::ChainIdentity)) { identity.chain_id += 1; }
        Ok(identity)
    }
    async fn code_hash(&self, _: Address, _: &BlockRef) -> Result<B256> {
        let mut data = self.0.lock().unwrap(); data.code_calls += 1;
        if data.code_calls > 1 && matches!(data.recheck_fault, Some(FinalityFault::ExecutorCode)) { return Ok(B256::repeat_byte(99)); }
        Ok(data.executor_code_hash)
    }
    async fn head(&self, commitment: Commitment) -> Result<BlockRef> {
        let mut data = self.0.lock().unwrap();
        let mut number = if commitment == Commitment::Finalized { data.finalized } else { data.latest };
        if commitment == Commitment::Finalized {
            data.finalized_calls += 1;
            if data.finalized_calls > 1 && matches!(data.recheck_fault, Some(FinalityFault::FinalizedRegression)) { number -= 1; }
            if data.finalized_calls > 1 && matches!(data.recheck_fault, Some(FinalityFault::FinalizedAdvance)) { number += 1; }
        }
        let mut header = BlockRef { commitment, ..data.blocks[&number] };
        if commitment == Commitment::Finalized && data.finalized_calls > 1 && matches!(data.recheck_fault, Some(FinalityFault::FinalizedHash)) { header.hash = B256::repeat_byte(99); }
        Ok(header)
    }
    async fn block(&self, number: u64) -> Result<Option<BlockRef>> {
        let mut data = self.0.lock().unwrap();
        data.block_calls.push(number);
        let rechecking_inclusion = number == 5 && data.block_calls.iter().filter(|called| **called == 5).count() > 1;
        if rechecking_inclusion && matches!(data.recheck_fault, Some(FinalityFault::MissingInclusion))
            || number == data.finalized && matches!(data.recheck_fault, Some(FinalityFault::MissingFinalized))
        { return Ok(None); }
        Ok(data.blocks.get(&number).copied().map(|mut header| {
            if data.invalid_final_anchor && number == data.finalized { header.hash = B256::repeat_byte(99); }
            if rechecking_inclusion { match data.recheck_fault {
                Some(FinalityFault::InclusionHash) => header.hash = B256::repeat_byte(99),
                Some(FinalityFault::InclusionParent) => header.parent_hash = B256::repeat_byte(99),
                Some(FinalityFault::InclusionTime) => header.timestamp += 1,
                _ => {},
            } }
            header
        }))
    }
    async fn receipt(&self, hash: B256) -> Result<Option<Receipt>> {
        let mut data = self.0.lock().unwrap();
        if data.fail_receipts { return Err(eyre!("local receipt transport unavailable")); }
        let count = data.receipt_calls.entry(hash).or_default(); *count += 1;
        let rechecking = *count > 1;
        Ok(data.receipts.get(&hash).cloned().map(|mut receipt| {
            if rechecking && matches!(data.recheck_fault, Some(FinalityFault::Receipt)) { receipt.gas_used += 1; }
            receipt
        }))
    }
    async fn nonce(&self, _: Address, pending: bool) -> Result<u64> {
        let data = self.0.lock().unwrap(); Ok(if pending { data.pending_nonce } else { data.nonce })
    }
    async fn balance(&self, _: Address) -> Result<U256> { Ok(U256::from(1_000_000_000_u64)) }
    async fn gas_price(&self) -> Result<U256> { Ok(U256::from(1)) }
    async fn simulate(&self, _: Address, _: &UnsignedTx, _: &BlockRef) -> Result<Bytes> {
        let mut data = self.0.lock().unwrap(); data.simulations += 1;
        if let Some(number) = data.advance_head_on_simulate { data.latest = number; }
        Ok(Bytes::new())
    }
    async fn estimate(&self, _: Address, _: &UnsignedTx, _: &BlockRef) -> Result<u64> { Ok(90_000) }
    async fn send_raw(&self, raw: &Bytes) -> Result<B256> {
        let mut data = self.0.lock().unwrap(); data.sent.push(raw.clone());
        if data.fail_send { return Err(eyre!("local send response timed out after acceptance")); }
        Ok(keccak256(raw))
    }
}

fn worker(attempts: Vec<SignedAttempt>, rpc: Arc<MockRpc>) -> ((), TxWorker<MockRpc>) {
    let _scope = ();
    let mut store = JournalStore::new(identity()).unwrap();
    let last = attempts.last().unwrap().clone();
    store.update(|journal| {
        journal.active = Some(Intent { reservation: last.envelope, kind: last.kind,
            phase: Phase::SignedUncertain, attempts, reserved_at: 101 }); Ok(())
    }).unwrap();
    let worker = TxWorker::new(store, rpc, 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    (_scope, worker)
}

async fn broadcast_liquidation(worker: &mut TxWorker<MockRpc>) -> Result<()> {
    let at = worker.rpc().head(Commitment::Latest).await?;
    let bound = worker.store.journal().active.as_ref().unwrap().attempts.last().unwrap().envelope.native_bound()?;
    let mut cost = native_cost(at, 1_000, 100, 1);
    cost.native_bound = bound; cost.fingerprint = cost.compute_fingerprint();
    worker.broadcast_with_cost(&cost, U256::ZERO).await
}

fn receipt(attempt: &SignedAttempt, succeeded: bool) -> Receipt {
    let logs = if let (AttemptKind::Liquidation(meta), true) = (&attempt.kind, succeeded) {
        let event = LiquidationExecutor::LiquidationExecuted { pool: meta.key.pool, borrower: meta.key.borrower,
            market: meta.key.market, planFingerprint: meta.plan_fingerprint, actualRepay: 80,
            collateralSeized: 100, collateralSpent: U256::from(100), assetProfit: U256::from(8) }.encode_log_data();
        vec![ReceiptLog { address: identity().executor, topics: event.topics().to_vec(), data: event.data }]
    } else { Vec::new() };
    Receipt { hash: attempt.hash, block_number: 5, block_hash: block(5).hash, succeeded,
        effective_gas_price: attempt.envelope.gas_price, gas_used: 21_000, logs }
}

fn rebind_fingerprint(attempt: &mut SignedAttempt) {
    if let AttemptKind::Liquidation(ref mut meta) = attempt.kind {
        meta.simulation_fingerprint = attempt.envelope.fingerprint().unwrap();
    }
}

#[tokio::test]
async fn real_signed_envelope_rejects_sender_nonce_input_gas_chain_and_raw_corruption() {
    let valid = liquidation_attempt(200).await;
    validate_attempt(&identity(), &valid).unwrap();
    let wrong_signer: PrivateKeySigner = "0000000000000000000000000000000000000000000000000000000000000002".parse().unwrap();
    let mut wrong_sender = valid.clone();
    wrong_sender.raw = wrong_sender.envelope.sign(&wrong_signer).await.unwrap();
    wrong_sender.hash = keccak256(&wrong_sender.raw);
    assert!(validate_attempt(&identity(), &wrong_sender).is_err());
    let mut wrong_nonce = valid.clone(); wrong_nonce.envelope.nonce += 1; rebind_fingerprint(&mut wrong_nonce);
    assert!(validate_attempt(&identity(), &wrong_nonce).is_err());
    let mut wrong_gas = valid.clone(); wrong_gas.envelope.gas_limit += 1; rebind_fingerprint(&mut wrong_gas);
    assert!(validate_attempt(&identity(), &wrong_gas).is_err());
    let mut wrong_chain = valid.clone(); wrong_chain.envelope.chain_id += 1; rebind_fingerprint(&mut wrong_chain);
    assert!(validate_attempt(&identity(), &wrong_chain).is_err());
    let mut wrong_input = valid.clone();
    let mut call = LiquidationExecutor::executeCall::abi_decode(&wrong_input.envelope.input).unwrap();
    call.request.borrower = Address::repeat_byte(33);
    wrong_input.envelope.input = call.abi_encode().into();
    if let AttemptKind::Liquidation(ref mut meta) = wrong_input.kind { meta.key.borrower = Address::repeat_byte(33); }
    rebind_fingerprint(&mut wrong_input);
    assert!(validate_attempt(&identity(), &wrong_input).is_err());
    let mut trailing_raw = valid.clone();
    let mut raw = trailing_raw.raw.to_vec(); raw.push(0); trailing_raw.raw = raw.into(); trailing_raw.hash = keccak256(&trailing_raw.raw);
    assert!(validate_attempt(&identity(), &trailing_raw).is_err());
    let mut trailing_call = valid.clone();
    let mut data = trailing_call.envelope.input.to_vec(); data.extend_from_slice(&[0; 32]); trailing_call.envelope.input = data.into();
    rebind_fingerprint(&mut trailing_call);
    assert!(validate_unsigned(&identity(), &trailing_call.envelope, &trailing_call.kind).is_err());
}

#[tokio::test]
async fn expired_signed_startup_never_rebroadcasts_and_preserves_raw_for_recovery() {
    let attempt = liquidation_attempt(105).await;
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Expired);
    assert!(worker.broadcast().await.is_err());
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    let store = worker.store;

    assert_eq!(store.journal().active.as_ref().unwrap().attempts, vec![attempt]);
}

#[tokio::test]
async fn expired_block_quote_preserves_signed_raw_while_running_without_rebroadcast() {
    let (envelope, mut kind) = liquidation_unsigned(200, 1);
    if let AttemptKind::Liquidation(ref mut meta) = kind { meta.max_valid_block = 9; }
    let attempt = signed(envelope, kind).await;
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().latest = 9;
    let (_scope, mut original) = worker(vec![attempt.clone()], rpc.clone());
    assert_eq!(original.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    let store = original.store;

    rpc.0.lock().unwrap().latest = 10;

    let before = store.journal().clone();
    let mut restored = TxWorker::new(store, rpc.clone(), 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    assert!(block(10).timestamp < 200); // Timestamp TTL remains valid; only block age expired.
    assert_eq!(startup_reconcile(&mut restored).await.unwrap(), ReconcileOutcome::Expired);
    assert!(restored.broadcast().await.is_err());
    assert_eq!(*restored.store.journal(), before);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    assert_eq!(restored.store.journal().active.as_ref().unwrap().attempts, vec![attempt]);
}

#[tokio::test]
async fn expiry_during_pinned_simulation_stops_send_and_preserves_signed_journal() {
    for expire_by_block in [false, true] {
        let (envelope, mut kind) = liquidation_unsigned(if expire_by_block { 200 } else { 110 }, 1);
        if expire_by_block { if let AttemptKind::Liquidation(ref mut meta) = kind { meta.max_valid_block = 10; } }
        let attempt = signed(envelope, kind).await;
        let rpc = MockRpc::new(); rpc.0.lock().unwrap().advance_head_on_simulate = Some(11);
        let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
        let before = worker.store.journal().clone();
        assert!(broadcast_liquidation(&mut worker).await.is_err());
        assert_eq!(rpc.0.lock().unwrap().simulations, 1, "initial head must permit the pinned simulation");
        assert!(rpc.0.lock().unwrap().sent.is_empty(), "successful old simulation cannot authorize expired send");
        assert_eq!(*worker.store.journal(), before);
        let restored = worker.store;

        assert_eq!(restored.journal().active.as_ref().unwrap().attempts, vec![attempt]);
    }
}

#[tokio::test]
async fn null_receipt_and_pending_nonce_do_not_release_signed_intent() {
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![liquidation_attempt(200).await], rpc.clone());
    let before = worker.store.journal().clone();
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    assert_eq!(*worker.store.journal(), before);
    assert!(worker.release_unsent().await.is_err());
    assert_eq!(*worker.store.journal(), before);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
}

#[tokio::test]
async fn send_timeout_retains_exact_raw_and_retry_rebroadcasts_same_bytes() {
    let attempt = liquidation_attempt(200).await;
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().fail_send = true;
    let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    assert!(broadcast_liquidation(&mut worker).await.is_err());
    assert_eq!(worker.store.journal().active.as_ref().unwrap().phase, Phase::SignedUncertain);
    let store = worker.store;
    rpc.0.lock().unwrap().fail_send = false;

    let mut restored = TxWorker::new(store, rpc.clone(), 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    broadcast_liquidation(&mut restored).await.unwrap();
    assert_eq!(rpc.0.lock().unwrap().sent, vec![attempt.raw.clone(), attempt.raw]);
    assert_eq!(restored.store.journal().active.as_ref().unwrap().phase, Phase::Broadcast);
}

#[tokio::test]
async fn receipt_rpc_error_is_atomic_and_does_not_discard_attempts() {
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![liquidation_attempt(200).await], rpc.clone());
    let before = worker.store.journal().clone(); rpc.0.lock().unwrap().fail_receipts = true;
    assert!(worker.reconcile().await.is_err());
    assert_eq!(*worker.store.journal(), before);
}

#[tokio::test]
async fn older_liquidation_finalizes_while_new_cancel_is_pending_and_charges_signed_limit() {
    let liquidation = liquidation_attempt(200).await;
    let cancel = cancellation_attempt().await;
    let rpc = MockRpc::new();
    { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(liquidation.hash, receipt(&liquidation, true)); }
    let (_scope, mut worker) = worker(vec![liquidation.clone(), cancel.clone()], rpc);
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(OutcomeKind::Liquidated));
    assert!(worker.is_idle());
    let outcome = &worker.store.journal().outcomes[0];
    assert_eq!(outcome.attempt, liquidation);
    assert_eq!(outcome.attempts, vec![outcome.attempt.clone(), cancel]);
    assert_eq!(outcome.native_charge, U256::from(100_000)); // gasUsed=21_000 is not Monad billing.
    assert_eq!(outcome.asset_gain, Some(U256::from(8)));
    assert_eq!(outcome.net_asset, None); // Unknown realised native FX cannot become zero cost.
}

#[tokio::test]
async fn finalized_cancel_is_typed_cancel_and_revert_is_charged_separately() {
    for success in [true, false] {
        let cancellation = cancellation_attempt().await;
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(cancellation.hash, receipt(&cancellation, success)); }
        let (_scope, mut worker) = worker(vec![cancellation], rpc);
        let expected = if success { OutcomeKind::Cancelled } else { OutcomeKind::Reverted };
        assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(expected));
        assert_eq!(worker.store.journal().outcomes[0].asset_gain, None);
        assert_eq!(worker.store.journal().outcomes[0].native_charge, U256::from(42_000));
    }
}

#[tokio::test]
async fn unfinalized_mining_reorg_restores_uncertain_intent_without_releasing_nonce() {
    let attempt = liquidation_attempt(200).await;
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().receipts.insert(attempt.hash, receipt(&attempt, true));
    let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::MinedUnfinalized);
    assert!(matches!(worker.store.journal().active.as_ref().unwrap().phase, Phase::Mined(_)));
    rpc.0.lock().unwrap().blocks.get_mut(&5).unwrap().hash = B256::repeat_byte(99);
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    assert_eq!(worker.store.journal().active.as_ref().unwrap().phase, Phase::SignedUncertain);
    assert_eq!(worker.store.journal().active.as_ref().unwrap().attempts, vec![attempt]);
    assert!(worker.store.journal().outcomes.is_empty());
}

#[tokio::test]
async fn changed_canonical_finalized_anchor_cannot_archive_success() {
        let attempt = liquidation_attempt(200).await;
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(attempt.hash, receipt(&attempt, true));
            chain.invalid_final_anchor = true; }
        let (_scope, mut worker) = worker(vec![attempt], rpc);
        let before = worker.store.journal().clone();
        assert!(worker.reconcile().await.is_err());
        assert_eq!(*worker.store.journal(), before);
}

#[tokio::test]
async fn trusted_rpc_recovery_is_constant_cost_after_a_million_blocks_of_downtime() {
    let attempt = liquidation_attempt(200).await;
    let rpc = MockRpc::new();
    {
        let mut chain = rpc.0.lock().unwrap();
        // Only the inclusion and current finalized header exist. Any ancestry
        // walk would encounter a missing header immediately. This test models
        // explicitly trusted RPC canonical/finalized attestations, not a proof.
        chain.blocks.clear();
        chain.blocks.insert(5, block(5));
        chain.blocks.insert(1_000_005, block(1_000_005));
        chain.latest = 1_000_005; chain.finalized = 1_000_005;
        chain.receipts.insert(attempt.hash, receipt(&attempt, true));
    }
    let (_scope, mut worker) = worker(vec![attempt], rpc.clone());
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(OutcomeKind::Liquidated));
    assert_eq!(rpc.0.lock().unwrap().block_calls, vec![5, 5, 1_000_005]);
    assert_eq!(worker.store.journal().outcome_count().unwrap(), 1);
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Idle);
    assert_eq!(worker.store.journal().outcome_count().unwrap(), 1);
}

#[tokio::test]
async fn finality_recovery_rejects_every_observed_recheck_inconsistency_atomically() {
    for fault in [FinalityFault::InclusionHash, FinalityFault::InclusionParent, FinalityFault::InclusionTime,
        FinalityFault::MissingInclusion, FinalityFault::MissingFinalized, FinalityFault::Receipt, FinalityFault::ChainIdentity, FinalityFault::ExecutorCode,
        FinalityFault::FinalizedRegression, FinalityFault::FinalizedHash]
    {
        let attempt = liquidation_attempt(200).await;
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.recheck_fault = Some(fault);
            chain.receipts.insert(attempt.hash, receipt(&attempt, true)); }
        let (_scope, mut worker) = worker(vec![attempt], rpc.clone());
        let before = worker.store.journal().clone();

        assert!(worker.reconcile().await.is_err(), "observed RPC inconsistency must not archive: {fault:?}");
        assert_eq!(*worker.store.journal(), before);
        assert!(rpc.0.lock().unwrap().sent.is_empty());
    }
}

#[tokio::test]
async fn finality_recovery_requires_real_headers_and_consistent_inclusion_metadata() {
    for defect in 0..6 {
        let attempt = liquidation_attempt(200).await;
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10;
            chain.receipts.insert(attempt.hash, receipt(&attempt, true));
            match defect {
                0 => { chain.blocks.remove(&5); },
                1 => chain.blocks.get_mut(&5).unwrap().number = 6,
                2 => chain.blocks.get_mut(&10).unwrap().timestamp = 104,
                3 => chain.blocks.get_mut(&10).unwrap().hash = B256::ZERO,
                4 => chain.receipts.get_mut(&attempt.hash).unwrap().hash = B256::repeat_byte(99),
                5 => chain.receipts.get_mut(&attempt.hash).unwrap().block_hash = B256::ZERO,
                _ => unreachable!(),
            }
        }
        let (_scope, mut worker) = worker(vec![attempt], rpc);
        let before = worker.store.journal().clone();
        assert!(worker.reconcile().await.is_err());
        assert_eq!(*worker.store.journal(), before);
    }
}

#[tokio::test]
async fn trusted_rpc_finality_can_progress_or_match_the_exact_inclusion_height() {
    for progresses in [false, true] {
        let attempt = liquidation_attempt(200).await;
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = if progresses { 10 } else { 5 };
            if progresses { chain.recheck_fault = Some(FinalityFault::FinalizedAdvance); }
            chain.receipts.insert(attempt.hash, receipt(&attempt, true)); }
        let (_scope, mut worker) = worker(vec![attempt], rpc.clone());
        assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(OutcomeKind::Liquidated));
        let expected = if progresses { vec![5, 5, 10, 11] } else { vec![5, 5, 5] };
        assert_eq!(rpc.0.lock().unwrap().block_calls, expected);
    }
}

fn now_unix_ms() -> u64 { u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap() }

#[tokio::test]
async fn cancellation_retry_origin_survives_rebroadcast_retry_and_exact_timeout_boundary() {
    let mut cancel = cancellation_attempt().await;
    let origin = now_unix_ms(); cancel.created_at_unix_ms = Some(origin);
    let rpc = MockRpc::new();
    let (_scope, mut worker) = worker(vec![cancel.clone()], rpc.clone());
    worker.set_cancellation_retry_seconds(86_400).unwrap();
    assert!(!worker.cancellation_retry_due(origin + 86_399_999).unwrap());
    assert!(worker.cancellation_retry_due(origin + 86_400_000).unwrap());
    assert!(worker.cancellation_retry_due(origin - 1).is_err());
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    worker.broadcast().await.unwrap(); worker.broadcast().await.unwrap();
    assert_eq!(worker.store.journal().active.as_ref().unwrap().attempts[0].created_at_unix_ms, Some(origin));
    let store = worker.store;

    let mut restored = TxWorker::new(store, rpc.clone(), 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    restored.set_cancellation_retry_seconds(86_400).unwrap();
    restored.broadcast().await.unwrap();
    let saved = &restored.store.journal().active.as_ref().unwrap().attempts[0];
    assert_eq!(saved.created_at_unix_ms, Some(origin));
    assert_eq!(saved.raw, cancel.raw); assert_eq!(saved.hash, cancel.hash);
    assert_eq!(rpc.0.lock().unwrap().sent, vec![cancel.raw.clone(), cancel.raw.clone(), cancel.raw]);
    let before = restored.store.journal().clone();
    assert!(restored.store.update(|j| { j.active.as_mut().unwrap().attempts[0].created_at_unix_ms = Some(origin + 1); Ok(()) }).is_err());
    assert!(restored.store.update(|j| { j.active.as_mut().unwrap().attempts[0].created_at_unix_ms = None; Ok(()) }).is_err());
    assert_eq!(*restored.store.journal(), before);
}

#[tokio::test]
async fn legacy_cancellation_adopts_one_process_timer_without_rewriting_signed_evidence() {
    let cancel = cancellation_attempt().await;
    assert!(!serde_json::to_string(&cancel).unwrap().contains("created_at_unix_ms"));
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![cancel.clone()], rpc.clone());
    let before = now_unix_ms();
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    let adopted = worker.store.journal().active.as_ref().unwrap().attempts[0].clone();
    assert!(adopted.created_at_unix_ms.unwrap() >= before);
    assert!(adopted.created_at_unix_ms.unwrap() <= now_unix_ms());
    let mut expected = cancel; expected.created_at_unix_ms = adopted.created_at_unix_ms;
    assert_eq!(adopted, expected); // Raw bytes, hash, chain timestamp and envelope are unchanged.
    worker.broadcast().await.unwrap(); let store = worker.store;

    let mut restored = TxWorker::new(store, rpc, 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    assert_eq!(restored.reconcile().await.unwrap(), ReconcileOutcome::Pending);
    assert_eq!(restored.store.journal().active.as_ref().unwrap().attempts[0].created_at_unix_ms, adopted.created_at_unix_ms);
}

#[tokio::test]
async fn cancellation_timeout_replaces_with_higher_fee_and_any_older_hash_can_still_win_once() {
    for older_cancel_wins in [false, true] {
        let liquidation = liquidation_attempt(105).await; // The original plan is already expired.
        let mut old_cancel = cancellation_attempt().await; old_cancel.created_at_unix_ms = Some(1);
        let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![liquidation.clone(), old_cancel.clone()], rpc.clone());
        worker.set_cancellation_retry_seconds(1).unwrap();
        assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::CancelReplacementDue);
        worker.broadcast().await.unwrap(); // A bounded replacement policy must still permit retry of existing cleanup bytes.
        assert!(worker.cancellation_retry_due(now_unix_ms()).unwrap()); // Resend never renews the retry origin.
        assert_eq!(rpc.0.lock().unwrap().sent, vec![old_cancel.raw.clone()]);
        old_cancel.broadcast_at = Some(block(10).timestamp);
        let price = worker.next_cancel_price(U256::from(2)).unwrap();
        assert_eq!(price, U256::from(3)); // Ceil 12.5% bump also increases tiny integer prices.
        worker.cancel(price, &signer()).await.unwrap();
        let history = worker.store.journal().active.as_ref().unwrap().attempts.clone();
        assert_eq!(history[..2], [liquidation.clone(), old_cancel.clone()]);
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].envelope.nonce, old_cancel.envelope.nonce);
        assert_eq!(history[2].envelope.gas_price, price);
        assert!(history[2].created_at_unix_ms.unwrap() > old_cancel.created_at_unix_ms.unwrap());
        worker.broadcast().await.unwrap();
        assert_eq!(rpc.0.lock().unwrap().sent, vec![old_cancel.raw.clone(), history[2].raw.clone()]);
        let history = worker.store.journal().active.as_ref().unwrap().attempts.clone();
        let winner = if older_cancel_wins { old_cancel } else { liquidation };
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(winner.hash, receipt(&winner, true)); }
        let expected = if older_cancel_wins { OutcomeKind::Cancelled } else { OutcomeKind::Liquidated };
        assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(expected));
        let outcome = &worker.store.journal().outcomes[0];
        assert_eq!(outcome.attempt, winner); assert_eq!(outcome.attempts, history);
        assert_eq!(outcome.native_charge, winner.envelope.native_bound().unwrap());
        assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Idle);
        assert_eq!(worker.store.journal().outcome_count().unwrap(), 1);
    }
}

#[tokio::test]
async fn cancellation_price_caps_capacity_and_native_reserve_fail_before_mutating_evidence() {
    for policy in 0..5 {
        let liquidation = liquidation_attempt(200).await;
        let mut cancel = cancellation_attempt().await; cancel.created_at_unix_ms = Some(1);
        let rpc = MockRpc::new(); let (_scope, original) = worker(vec![liquidation, cancel], rpc.clone());
        let mut worker = TxWorker::new(original.store, rpc.clone(), if policy == 0 { 2 } else { 8 },
            U256::from(if policy == 1 { 20_000 } else { 10_000_000 }),
            U256::from(if policy == 2 { 62_999 } else { 100_000 })).unwrap();
        if policy == 3 { worker.set_max_gas_price(U256::from(2)).unwrap(); }
        let before = worker.store.journal().clone();
        let price = if policy == 4 { U256::from(2) } else { U256::from(3) };
        assert!(worker.cancel(price, &signer()).await.is_err());
        assert_eq!(*worker.store.journal(), before);
        assert!(rpc.0.lock().unwrap().sent.is_empty());
    }
    let mut cancel = cancellation_attempt().await; cancel.created_at_unix_ms = Some(1);
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![cancel], rpc);
    assert!(worker.next_cancel_price(U256::ZERO).is_err());
    assert_eq!(worker.next_cancel_price(U256::from(7)).unwrap(), U256::from(7));
    worker.set_max_gas_price(U256::from(6)).unwrap();
    assert!(worker.next_cancel_price(U256::from(7)).is_err());
    assert!(worker.set_cancellation_retry_seconds(0).is_err());
    assert!(worker.set_cancellation_retry_seconds(86_401).is_err());
}

#[tokio::test]
async fn cancellation_fee_rounding_rejects_a_small_increase_and_legacy_integer_overflow() {
    let mut envelope = cancellation_attempt().await.envelope; envelope.gas_price = U256::from(9);
    let mut cancel = signed(envelope, AttemptKind::Cancel).await; cancel.created_at_unix_ms = Some(1);
    let rpc = MockRpc::new(); let (_scope, original) = worker(vec![cancel], rpc.clone());
    let mut active = TxWorker::new(original.store, rpc.clone(), 8, U256::from(10_000_000), U256::from(1_000_000)).unwrap();
    assert_eq!(active.next_cancel_price(U256::from(9)).unwrap(), U256::from(11));
    let before = active.store.journal().clone();
    assert!(active.cancel(U256::from(10), &signer()).await.unwrap_err().to_string().contains("replacement gas price exceeds policy"));
    assert_eq!(*active.store.journal(), before);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    let mut envelope = cancellation_attempt().await.envelope; envelope.gas_price = U256::from(u128::MAX);
    let cancel = signed(envelope, AttemptKind::Cancel).await;
    let rpc = MockRpc::new(); let (_scope, worker) = worker(vec![cancel], rpc);
    assert!(worker.next_cancel_price(U256::from(1)).is_err()); // Rounded bump cannot fit a signed legacy fee.
}

#[tokio::test]
async fn exhausted_cancel_attempt_budget_can_rebroadcast_existing_due_raw_after_transport_recovers() {
    let liquidation = liquidation_attempt(105).await;
    let mut cancel = cancellation_attempt().await; cancel.created_at_unix_ms = Some(1);
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().fail_send = true;
    let (_scope, original) = worker(vec![liquidation.clone(), cancel.clone()], rpc.clone());
    let mut worker = TxWorker::new(original.store, rpc.clone(), 2, U256::from(10_000_000), U256::from(100_000)).unwrap();
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::CancelReplacementDue);
    let before = worker.store.journal().clone();
    assert!(worker.cancel(U256::from(3), &signer()).await.unwrap_err().to_string().contains("attempt budget exhausted"));
    assert_eq!(*worker.store.journal(), before);
    assert!(worker.broadcast().await.is_err()); // Failed response does not permit discarding signed cleanup evidence.
    rpc.0.lock().unwrap().fail_send = false;
    worker.broadcast().await.unwrap();
    let active = worker.store.journal().active.as_ref().unwrap();
    assert_eq!(active.attempts.len(), 2); assert_eq!(active.attempts[0], liquidation);
    assert_eq!(active.attempts[1].hash, cancel.hash); assert_eq!(active.attempts[1].raw, cancel.raw);
    assert_eq!(active.attempts[1].created_at_unix_ms, Some(1));
    assert_eq!(rpc.0.lock().unwrap().sent, vec![cancel.raw.clone(), cancel.raw.clone()]);
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::CancelReplacementDue);
    { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(cancel.hash, receipt(&cancel, true)); }
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(OutcomeKind::Cancelled));
    assert_eq!(worker.store.journal().outcome_count().unwrap(), 1);
    assert_eq!(worker.store.journal().native_spent().unwrap(), U256::from(42_000));
    assert_eq!(worker.store.journal().outcomes[0].attempts.len(), 2);
}

#[tokio::test]
async fn externally_consumed_unknown_nonce_is_not_guessed_or_rebroadcast() {
    let rpc = MockRpc::new(); rpc.0.lock().unwrap().nonce = 8;
    let (_scope, mut worker) = worker(vec![liquidation_attempt(200).await], rpc.clone());
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::UnknownNonceConsumed);
    assert_eq!(worker.store.journal().active.as_ref().unwrap().phase, Phase::UnknownNonceConsumed);
    assert!(worker.broadcast().await.is_err());
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    assert!(worker.store.journal().outcomes.is_empty());
}

#[tokio::test]
async fn wrong_executor_event_cannot_be_reported_as_successful_liquidation() {
    let attempt = liquidation_attempt(200).await;
    let mut receipt = receipt(&attempt, true); receipt.logs[0].address = Address::repeat_byte(99);
    let rpc = MockRpc::new();
    { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(attempt.hash, receipt); }
    let (_scope, mut worker) = worker(vec![attempt], rpc);
    assert_eq!(worker.reconcile().await.unwrap(), ReconcileOutcome::Finalized(OutcomeKind::InvalidSuccessEvent));
    assert_eq!(worker.store.journal().outcomes[0].asset_gain, None);
    assert_eq!(worker.store.journal().outcomes[0].native_charge, U256::from(100_000));
}

#[tokio::test]
async fn startup_discovered_invalid_success_stops_work_and_archives_evidence_in_memory() {
    let attempt = liquidation_attempt(200).await;
    let mut invalid_receipt = receipt(&attempt, true); invalid_receipt.logs.clear();
    let rpc = MockRpc::new();
    { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(attempt.hash, invalid_receipt); }
    let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    assert!(worker.store.journal().outcomes.is_empty());
    assert!(startup_reconcile(&mut worker).await.is_err());
    assert!(worker.is_idle()); // Inclusion is terminal, but the operator gate forbids new work.
    assert_eq!(worker.store.journal().outcomes.len(), 1);
    assert_eq!(worker.store.journal().outcomes[0].kind, OutcomeKind::InvalidSuccessEvent);
    assert_eq!(worker.store.journal().outcomes[0].attempt, attempt);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    let store = worker.store;

    assert!(ensure_operator_gate(store.journal()).is_err());
    let mut restored = TxWorker::new(store, rpc.clone(), 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    assert!(startup_reconcile(&mut restored).await.is_err());
    assert_eq!(restored.store.journal().outcomes.len(), 1); // No duplicate archive/repeated send.
    assert!(rpc.0.lock().unwrap().sent.is_empty());
}

async fn attempt_for_state(nonce: u64, state_fingerprint: B256) -> SignedAttempt {
    let (mut envelope, mut kind) = liquidation_unsigned(200, 1);
    envelope.nonce = nonce;
    if let AttemptKind::Liquidation(ref mut meta) = kind {
        meta.simulation_fingerprint = envelope.fingerprint().unwrap();
        meta.paid_state_fingerprint = state_fingerprint;
    }
    signed(envelope, kind).await
}

#[tokio::test]
async fn paid_failure_counts_survive_retry_and_separate_state_changes_cancels_and_success() {
    let original = B256::repeat_byte(10);
    let changed = B256::repeat_byte(11);
    let first = attempt_for_state(7, original).await;
    let key = match &first.kind { AttemptKind::Liquidation(meta) => meta.key, _ => unreachable!() };
    let revert = audit_outcome(&identity(), &first, &receipt(&first, false), vec![first.clone()], None).unwrap();
    let before_cancel = attempt_for_state(8, original).await;
    let mut cancel_envelope = cancellation_attempt().await.envelope; cancel_envelope.nonce = 8;
    let cancel = signed(cancel_envelope, AttemptKind::Cancel).await;
    let cancelled = audit_outcome(&identity(), &cancel, &receipt(&cancel, true), vec![before_cancel, cancel.clone()], None).unwrap();
    let success = attempt_for_state(9, original).await;
    let liquidated = audit_outcome(&identity(), &success, &receipt(&success, true), vec![success.clone()], None).unwrap();
    let changed_attempt = attempt_for_state(10, changed).await;
    let changed_revert = audit_outcome(&identity(), &changed_attempt, &receipt(&changed_attempt, false), vec![changed_attempt.clone()], None).unwrap();
    let _scope = ();
    let mut store = JournalStore::new(identity()).unwrap();
    store.update(|journal| { journal.outcomes.extend([revert, cancelled, liquidated, changed_revert]); Ok(()) }).unwrap();
    assert_eq!(store.paid_failure_count(&key, original).await.unwrap(), 2);
    assert_eq!(store.paid_failure_count(&key, changed).await.unwrap(), 1);
    let mut restored = store;
    assert_eq!(restored.paid_failure_count(&key, original).await.unwrap(), 2);
    assert_eq!(restored.paid_failure_count(&key, changed).await.unwrap(), 1);
    let mut unrelated = key; unrelated.borrower = Address::repeat_byte(88);
    assert_eq!(restored.paid_failure_count(&unrelated, original).await.unwrap(), 0);
    assert_eq!(restored.paid_failure_count(&key, B256::repeat_byte(12)).await.unwrap(), 0);
    assert_eq!(restored.journal().outcomes.len(), 1);
    assert_eq!(restored.journal().outcome_count().unwrap(), 4);
    assert_eq!(restored.journal().native_spent().unwrap(), U256::from(342_000));
    // Returning to an earlier fingerprint must not reset its paid failures.
    let repeat = attempt_for_state(11, original).await;
    let outcome = audit_outcome(&identity(), &repeat, &receipt(&repeat, false), vec![repeat.clone()], None).unwrap();
    restored.update(|j| { j.outcomes.push(outcome); Ok(()) }).unwrap();
    assert_eq!(restored.paid_failure_count(&key, original).await.unwrap(), 3);
    assert_eq!(restored.journal().native_spent().unwrap(), U256::from(442_000));
    restored.retain_failures(&Default::default());
    assert_eq!(restored.paid_failure_count(&key, original).await.unwrap(), 0);
    assert_eq!(restored.journal().native_spent().unwrap(), U256::from(442_000));
    // A new process has no retained history, by design.
    let fresh = JournalStore::new(identity()).unwrap();
    assert_eq!(fresh.journal().native_spent().unwrap(), U256::ZERO);
    assert!(fresh.journal().active.is_none());
}

#[tokio::test]
async fn lowered_retry_total_and_price_limits_block_saved_send_without_losing_raw() {
    let (envelope, kind) = liquidation_unsigned(200, 2);
    let attempt = signed(envelope, kind).await;
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    let earlier = attempt_for_state(6, B256::repeat_byte(10)).await;
    let charged = audit_outcome(&identity(), &earlier, &receipt(&earlier, false), vec![earlier.clone()], None).unwrap();
    worker.store.update(|journal| { journal.outcomes.push(charged); Ok(()) }).unwrap();
    let before = worker.store.journal().clone();
    let store = worker.store;

    // 100_000 paid + 200_000 saved liability + 10_000 cleanup reserve = 310_000.
    ensure_send_budget(store.journal(), U256::from(310_000), U256::from(2), 300_000, U256::from(10_000)).unwrap();
    assert!(ensure_send_budget(store.journal(), U256::from(309_999), U256::from(2), 300_000, U256::from(10_000)).is_err());
    assert!(ensure_send_budget(store.journal(), U256::from(500_000), U256::from(1), 300_000, U256::from(10_000)).is_err());
    assert!(ensure_send_budget(store.journal(), U256::from(500_000), U256::from(2), 99_999, U256::from(10_000)).is_err());
    assert_eq!(*store.journal(), before);
    let mut restored = TxWorker::new(store, rpc.clone(), 8, U256::from(10_000_000), U256::from(100_000)).unwrap();
    restored.set_max_gas_price(U256::from(1)).unwrap();
    assert!(broadcast_liquidation(&mut restored).await.is_err()); // Worker independently enforces the new fee cap.
    assert_eq!(*restored.store.journal(), before);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    assert_eq!(restored.store.journal().active.as_ref().unwrap().attempts[0].raw, attempt.raw);
}

#[tokio::test]
async fn native_cost_expiring_during_simulation_never_sends_or_renews_signed_plan() {
    for expire_by_block in [false, true] {
        let attempt = liquidation_with_profit(200, 2).await;
        let rpc = MockRpc::new(); rpc.0.lock().unwrap().advance_head_on_simulate = Some(11);
        let cost = native_cost(block(10), if expire_by_block { 200 } else { 111 }, if expire_by_block { 0 } else { 20 }, 1);
        let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
        let before = worker.store.journal().clone();
        let error = worker.broadcast_with_cost(&cost, U256::from(1)).await.unwrap_err();
        assert!(error.to_string().contains("quote block/time limits exceeded"), "original plan remains valid; only cost quote expired: {error}");
        assert_eq!(rpc.0.lock().unwrap().simulations, 1);
        assert!(rpc.0.lock().unwrap().sent.is_empty());
        assert_eq!(*worker.store.journal(), before);
        let restored = worker.store;

        assert_eq!(*restored.journal(), before, "signed bytes, profit, deadline and max block must survive the failed send unchanged");
    }
}

#[tokio::test]
async fn already_stale_or_unbound_native_cost_is_rejected_before_simulation() {
    let attempt = liquidation_with_profit(200, 2).await;
    let valid = native_cost(block(10), 200, 20, 1);
    let mut stale = native_cost(block(9), 110, 20, 1);
    stale.fingerprint = stale.compute_fingerprint();
    let mut wrong_asset = valid.clone(); wrong_asset.asset = Address::repeat_byte(99); wrong_asset.fingerprint = wrong_asset.compute_fingerprint();
    let mut wrong_bound = valid.clone(); wrong_bound.native_bound += U256::from(1); wrong_bound.fingerprint = wrong_bound.compute_fingerprint();
    let mut tampered = valid; tampered.cost_asset += U256::from(1);
    for cost in [stale, wrong_asset, wrong_bound, tampered] {
        let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
        let before = worker.store.journal().clone();
        assert!(worker.broadcast_with_cost(&cost, U256::from(1)).await.is_err());
        assert_eq!(rpc.0.lock().unwrap().simulations, 0);
        assert!(rpc.0.lock().unwrap().sent.is_empty());
        assert_eq!(*worker.store.journal(), before);
    }
}

#[tokio::test]
async fn native_cost_orphan_anchor_blocks_send_despite_valid_time_and_profit() {
    let attempt = liquidation_with_profit(200, 2).await;
    let rpc = MockRpc::new();
    let mut orphan = block(9); orphan.hash = B256::repeat_byte(99);
    let cost = native_cost(orphan, 200, 20, 1);
    let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    let before = worker.store.journal().clone();
    let error = worker.broadcast_with_cost(&cost, U256::from(1)).await.unwrap_err();
    assert!(error.to_string().contains("canonical RPC anchor changed"));
    assert_eq!(rpc.0.lock().unwrap().simulations, 1);
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    assert_eq!(*worker.store.journal(), before);
    let restored = worker.store;

    assert_eq!(restored.journal().active.as_ref().unwrap().attempts, vec![attempt]);
}

#[tokio::test]
async fn fresh_cost_and_allowance_control_same_raw_resend_without_extending_guards() {
    let attempt = liquidation_with_profit(200, 3).await;
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![attempt.clone()], rpc.clone());
    worker.broadcast_with_cost(&native_cost(block(10), 200, 20, 1), U256::from(1)).await.unwrap();
    worker.broadcast_with_cost(&native_cost(block(10), 200, 20, 2), U256::from(1)).await.unwrap();
    assert_eq!(rpc.0.lock().unwrap().sent, vec![attempt.raw.clone(), attempt.raw.clone()]);
    let before = worker.store.journal().clone();
    assert_eq!(before.active.as_ref().unwrap().attempts[0].kind, attempt.kind);
    assert_eq!(before.active.as_ref().unwrap().attempts[0].envelope, attempt.envelope);
    let error = worker.broadcast_with_cost(&native_cost(block(10), 200, 20, 3), U256::from(1)).await.unwrap_err();
    assert!(error.to_string().contains("does not cover current native cost and allowance"));
    assert_eq!(rpc.0.lock().unwrap().sent.len(), 2);
    assert_eq!(*worker.store.journal(), before);
}

#[tokio::test]
async fn saved_cancel_budget_still_covers_the_older_liquidation_that_can_win() {
    let (envelope, kind) = liquidation_unsigned(200, 2);
    let liquidation = signed(envelope, kind).await;
    let mut cancel_envelope = cancellation_attempt().await.envelope;
    cancel_envelope.gas_price = U256::from(3);
    let cancel = signed(cancel_envelope, AttemptKind::Cancel).await;
    let rpc = MockRpc::new();
    let (_scope, worker) = worker(vec![liquidation.clone(), cancel.clone()], rpc);
    let journal = worker.store.journal();
    assert_eq!(cancel.envelope.native_bound().unwrap(), U256::from(63_000));
    assert_eq!(barterlend_liquidator::app::max_pending_native_bound(journal).unwrap(), U256::from(200_000));
    assert!(ensure_send_budget(journal, U256::from(199_999), U256::from(3), 200_000, U256::from(10_000)).is_err());
    ensure_send_budget(journal, U256::from(200_000), U256::from(3), 200_000, U256::from(10_000)).unwrap();
    assert_eq!(journal.active.as_ref().unwrap().attempts, vec![liquidation, cancel]);
}

#[tokio::test]
async fn liquidation_without_native_cost_quote_cannot_bypass_profit_guard() {
    let attempt = liquidation_attempt(200).await;
    let rpc = MockRpc::new(); let (_scope, mut worker) = worker(vec![attempt], rpc.clone());
    let before = worker.store.journal().clone();
    let error = worker.broadcast().await.unwrap_err();
    assert!(error.to_string().contains("liquidation requires a current native cost quote"));
    assert!(rpc.0.lock().unwrap().sent.is_empty());
    assert_eq!(rpc.0.lock().unwrap().simulations, 0);
    assert_eq!(*worker.store.journal(), before);
}

#[tokio::test]
async fn legacy_receipt_price_zero_lower_or_higher_than_signed_is_rejected_atomically() {
    let (envelope, kind) = liquidation_unsigned(200, 2); let attempt = signed(envelope, kind).await;
    for price in [0, 1, 3] {
        let mut incorrect = receipt(&attempt, true); incorrect.effective_gas_price = U256::from(price);
        assert!(audit_outcome(&identity(), &attempt, &incorrect, vec![attempt.clone()], None).is_err());
        let rpc = MockRpc::new();
        { let mut chain = rpc.0.lock().unwrap(); chain.finalized = 10; chain.receipts.insert(attempt.hash, incorrect); }
        let (_scope, mut worker) = worker(vec![attempt.clone()], rpc);
        let before = worker.store.journal().clone();
        assert!(worker.reconcile().await.is_err());
        assert_eq!(*worker.store.journal(), before);
    }
}

#[tokio::test]
async fn outcome_validation_rederives_event_classification_gain_and_nonce_history() {
    let attempt = liquidation_attempt(200).await;
    let valid = audit_outcome(&identity(), &attempt, &receipt(&attempt, true), vec![attempt.clone()], None).unwrap();
    let mut journal = Journal::new(identity()); journal.outcomes.push(valid.clone()); journal.validate().unwrap();
    let mut incorrect_kind = journal.clone(); incorrect_kind.outcomes[0].kind = OutcomeKind::Cancelled; assert!(incorrect_kind.validate().is_err());
    let mut inflated_gain = journal.clone(); inflated_gain.outcomes[0].asset_gain = Some(U256::from(999)); assert!(inflated_gain.validate().is_err());
    let mut duplicated_nonce = journal.clone(); duplicated_nonce.outcomes.push(valid.clone()); assert!(duplicated_nonce.validate().is_err());
    let other = attempt_for_state(8, B256::repeat_byte(10)).await;
    let mut unrelated_history = journal.clone(); unrelated_history.outcomes[0].attempts.push(other); assert!(unrelated_history.validate().is_err());
    let mut duplicated_hash = journal; duplicated_hash.outcomes[0].attempts.push(attempt); assert!(duplicated_hash.validate().is_err());
}

#[tokio::test]
async fn noncanonical_receipts_never_recognize_profit_and_keep_paid_charge() {
    let attempt = liquidation_attempt(200).await;
    for variant in ["extra_word", "dirty_amount", "dirty_address", "extra_topic", "missing_topic", "truncated"] {
        let mut invalid = receipt(&attempt, true);
        let log = &mut invalid.logs[0];
        let mut bytes = log.data.to_vec();
        match variant {
            "extra_word" => bytes.extend_from_slice(&[0; 32]),
            "dirty_amount" => bytes[32] = 1,
            "dirty_address" => log.topics[1].0[0] = 1,
            "extra_topic" => log.topics.push(B256::ZERO),
            "missing_topic" => { log.topics.pop(); },
            "truncated" => { bytes.pop(); },
            _ => unreachable!(),
        }
        log.data = bytes.into();
        let outcome = audit_outcome(&identity(), &attempt, &invalid, vec![attempt.clone()], None).unwrap();
        assert_eq!(outcome.kind, OutcomeKind::InvalidSuccessEvent, "{variant}");
        assert!(outcome.asset_gain.is_none() && outcome.net_asset.is_none());
        assert_eq!(outcome.native_charge, attempt.envelope.native_bound().unwrap());
    }
}

#[tokio::test]
async fn journal_updates_cannot_drop_or_rewrite_signed_nonce_evidence() {
    let first = liquidation_attempt(200).await; let rpc = MockRpc::new();
    let (_scope, mut worker) = worker(vec![first.clone()], rpc);
    let before = worker.store.journal().clone();
    assert!(worker.store.update(|journal| { journal.active = None; Ok(()) }).is_err());
    let replacement = cancellation_attempt().await;
    assert!(worker.store.update(|journal| {
        let active = journal.active.as_mut().unwrap(); active.attempts = vec![replacement.clone()];
        active.reservation = replacement.envelope.clone(); active.kind = replacement.kind.clone(); Ok(())
    }).is_err());
    assert!(worker.store.update(|journal| { journal.active.as_mut().unwrap().reserved_at += 1; Ok(()) }).is_err());
    assert_eq!(*worker.store.journal(), before);
    // A replacement can be appended and an older winner can finalize, preserving both signatures.
    worker.store.update(|journal| {
        let active = journal.active.as_mut().unwrap(); active.attempts.push(replacement.clone());
        active.reservation = replacement.envelope.clone(); active.kind = replacement.kind.clone(); Ok(())
    }).unwrap();
    let history = worker.store.journal().active.as_ref().unwrap().attempts.clone();
    let terminal = audit_outcome(&identity(), &first, &receipt(&first, true), history.clone(), None).unwrap();
    worker.store.update(|journal| { journal.active = None; journal.outcomes.push(terminal); Ok(()) }).unwrap();
    assert_eq!(worker.store.journal().outcomes[0].attempts, history);
}

#[tokio::test]
async fn archived_paid_charge_is_append_only_and_invalid_success_cannot_be_hidden() {
    let attempt = liquidation_attempt(200).await; let mut invalid = receipt(&attempt, true); invalid.logs.clear();
    let terminal = audit_outcome(&identity(), &attempt, &invalid, vec![attempt.clone()], None).unwrap();
    let _scope = ();
    let mut store = JournalStore::new(identity()).unwrap();
    store.update(|journal| { journal.outcomes.push(terminal.clone()); Ok(()) }).unwrap();
    let before = store.journal().clone();
    assert!(store.update(|journal| { journal.outcomes.clear(); Ok(()) }).is_err());
    // Even a separately valid rewritten Reverted outcome may not erase the operator gate.
    let reverted = audit_outcome(&identity(), &attempt, &receipt(&attempt, false), vec![attempt.clone()], None).unwrap();
    assert!(store.update(|journal| { journal.outcomes[0] = reverted; Ok(()) }).is_err());
    assert_eq!(*store.journal(), before);
}

#[test]
fn local_transport_rejects_mismatched_reader_endpoint_before_any_rpc() {
    use barterlend_state::{HttpReader, rpc::RpcConfig};
    use barterlend_liquidator::tx::rpc::LocalTxRpc;
    let reader = Arc::new(HttpReader::new("http://127.0.0.1:9/read", RpcConfig::default()).unwrap());
    assert!(LocalTxRpc::new("http://127.0.0.1:10/read", reader.clone()).is_err());
    assert!(LocalTxRpc::new("http://127.0.0.1:9/write", reader.clone()).is_err());
    assert!(LocalTxRpc::new("http://127.0.0.1:9/read", reader).is_ok());
}

#[tokio::test]
async fn direct_local_transport_rejects_nonlocal_raw_signature_before_rpc() {
    use barterlend_state::{HttpReader, rpc::RpcConfig};
    use barterlend_liquidator::tx::rpc::LocalTxRpc;
    let url = "http://127.0.0.1:9";
    let reader = Arc::new(HttpReader::new(url, RpcConfig::default()).unwrap());
    let rpc = LocalTxRpc::new(url, reader).unwrap();
    let mut envelope = cancellation_attempt().await.envelope; envelope.chain_id = 1;
    let raw = envelope.sign(&signer()).await.unwrap();
    let error = rpc.send_raw(&raw).await.unwrap_err();
    assert!(error.to_string().contains("canonical chain31337 legacy transactions"));
}

#[test]
fn executor_v2_quote_guard_is_bound_to_metadata_and_cannot_be_downgraded_to_v1() {
    let (tx,mut kind)=liquidation_unsigned(1000,1);
    validate_unsigned(&identity(),&tx,&kind).unwrap();
    if let AttemptKind::Liquidation(meta)=&mut kind {meta.min_swap_output=Some(U256::from(2));}
    assert!(validate_unsigned(&identity(),&tx,&kind).is_err());
    if let AttemptKind::Liquidation(meta)=&mut kind {meta.min_swap_output=None;}
    assert!(validate_unsigned(&identity(),&tx,&kind).is_err());
}

#[tokio::test]
async fn receipt_below_signed_quote_floor_is_invalid_even_with_positive_profit() {
    let (mut tx,mut kind)=liquidation_unsigned(1000,1);
    let mut call=LiquidationExecutor::executeCall::abi_decode(&tx.input).unwrap();
    call.request.minSwapOutput=U256::from(1000);tx.input=call.abi_encode().into();
    if let AttemptKind::Liquidation(meta)=&mut kind {meta.min_swap_output=Some(U256::from(1000));meta.simulation_fingerprint=tx.fingerprint().unwrap();}
    let attempt=signed(tx,kind).await;let receipt=receipt(&attempt,true);
    let outcome=audit_outcome(&identity(),&attempt,&receipt,vec![attempt.clone()],None).unwrap();
    assert_eq!(outcome.kind,OutcomeKind::InvalidSuccessEvent);assert_eq!(outcome.asset_gain,None);
}

#[tokio::test]
async fn public_terminal_evidence_never_contains_signed_bytes_or_swap_calldata() {
    let attempt=liquidation_attempt(500).await;
    let outcome=audit_outcome(&identity(),&attempt,&receipt(&attempt,true),vec![attempt.clone()],None).unwrap();
    let value=barterlend_liquidator::diagnostics::public_outcome(&outcome);
    let encoded=serde_json::to_string(&value).unwrap();
    assert!(!encoded.contains(&attempt.raw.to_string()));
    assert!(!encoded.contains(&attempt.envelope.input.to_string()));
    assert!(value["attempt"].get("raw").is_none());
    assert!(value["attempt"]["envelope"].get("input").is_none());
    assert_eq!(value["attempt"]["hash"],serde_json::to_value(attempt.hash).unwrap());
    assert_eq!(value["native_charge"],serde_json::to_value(outcome.native_charge).unwrap());
}
