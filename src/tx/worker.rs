use std::{sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use alloy::{primitives::{Bytes, U256, keccak256}, signers::local::PrivateKeySigner};
use barterlend_state::{BlockRef, Commitment};
use color_eyre::eyre::{Result, eyre};
use crate::{audit::audit_outcome, economics::CostQuote, simulation::SimulatedPlan};
use super::{*, rpc::TxRpc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome { Idle, ReservedNeedsRefresh, Pending, Expired, CancelReplacementDue, MinedUnfinalized, Finalized(OutcomeKind), UnknownNonceConsumed }

pub struct TxWorker<R: TxRpc> { pub store: JournalStore, rpc: Arc<R>, max_attempts: usize, max_native_bound: U256, max_gas_price: U256, cancel_reserve: U256, cancellation_retry_seconds: u64, signing: Arc<std::sync::atomic::AtomicBool> }

fn wall_time_ms() -> Result<u64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| eyre!("system clock precedes Unix epoch"))?;
    let now = u64::try_from(elapsed.as_millis()).map_err(|_| eyre!("system wall clock overflows u64"))?;
    if now == 0 { return Err(eyre!("invalid system wall clock")); }
    Ok(now)
}

fn ensure_nonce_continuity(journal: &Journal, nonce: u64) -> Result<()> {
    let previous = journal.outcomes.last().map(|outcome| outcome.attempt.envelope.nonce).or(journal.archive.last_nonce);
    if let Some(previous) = previous {
        let expected = previous.checked_add(1).ok_or_else(|| eyre!("terminal signer nonce exhausted"))?;
        if nonce != expected { return Err(eyre!("signer nonce skips session terminal history")); }
    }
    // An initial ledger has no historical cursor. Deploy/setup transactions can
    // already have used a nonzero nonce before the dedicated bot starts.
    Ok(())
}

impl<R: TxRpc> TxWorker<R> {
    pub fn new(store: JournalStore, rpc: Arc<R>, max_attempts: usize, max_native_bound: U256, cancel_reserve: U256) -> Result<Self> {
        if max_attempts < 2 || max_attempts > 16 || max_native_bound == U256::ZERO || cancel_reserve == U256::ZERO { return Err(eyre!("invalid transaction budgets")); }
        Ok(Self { store, rpc, max_attempts, max_native_bound, max_gas_price:U256::from(u128::MAX), cancel_reserve, cancellation_retry_seconds: 30, signing: Arc::new(std::sync::atomic::AtomicBool::new(true)) })
    }
    pub fn set_signing_gate(&mut self, signing: Arc<std::sync::atomic::AtomicBool>) { self.signing = signing; }
    fn ensure_signing(&self) -> Result<()> {
        if !self.signing.load(std::sync::atomic::Ordering::Acquire) { return Err(eyre!("shutdown forbids new signatures")); }
        Ok(())
    }
    pub fn is_idle(&self) -> bool { self.store.journal().active.is_none() }
    pub fn rpc(&self) -> &Arc<R> { &self.rpc }
    pub fn set_max_gas_price(&mut self, price: U256) -> Result<()> {
        if price == U256::ZERO || u128::try_from(price).is_err() { return Err(eyre!("invalid legacy gas price cap")); }
        self.max_gas_price = price; Ok(())
    }
    pub fn set_cancellation_retry_seconds(&mut self, seconds: u64) -> Result<()> {
        if !(1..=86_400).contains(&seconds) { return Err(eyre!("invalid cancellation retry interval")); }
        self.cancellation_retry_seconds = seconds; Ok(())
    }
    /// The in-memory origin survives response loss and rebroadcast within this process.
    /// A backwards wall clock cannot authorize an immediate replacement.
    pub fn cancellation_retry_due(&self, now_unix_ms: u64) -> Result<bool> {
        let Some(active) = self.store.journal().active.as_ref() else { return Ok(false); };
        if matches!(active.phase, Phase::ReservedUnsent | Phase::Mined(_) | Phase::UnknownNonceConsumed) { return Ok(false); }
        let Some(last) = active.attempts.last().filter(|last| matches!(last.kind, AttemptKind::Cancel)) else { return Ok(false); };
        let origin = last.created_at_unix_ms.ok_or_else(|| eyre!("cancellation retry timer has not been durably adopted"))?;
        let age = now_unix_ms.checked_sub(origin).ok_or_else(|| eyre!("cancellation wall clock moved backwards"))?;
        Ok(age >= self.cancellation_retry_seconds * 1_000)
    }
    /// Ceil 12.5% replacement bump and the current network recommendation; never
    /// clamp to a cap because an underpriced replacement would keep the nonce stuck.
    pub fn next_cancel_price(&self, network_price: U256) -> Result<U256> {
        if network_price.is_zero() { return Err(eyre!("current cancellation gas price is unavailable")); }
        let old = self.store.journal().active.as_ref().and_then(|active| active.attempts.last())
            .ok_or_else(|| eyre!("no signed cancellation source attempt"))?.envelope.gas_price;
        let bumped = old.checked_mul(U256::from(11_250)).and_then(|v| v.checked_add(U256::from(9_999)))
            .map(|v| v / U256::from(10_000)).ok_or_else(|| eyre!("replacement fee overflow"))?;
        let price = network_price.max(bumped);
        if price <= old || price > self.max_gas_price || u128::try_from(price).is_err() {
            return Err(eyre!("cancellation replacement gas price exceeds policy"));
        }
        Ok(price)
    }
    pub async fn verify_identity(&mut self) -> Result<()> {
        let identity = &self.store.journal().identity;
        if self.rpc.identity().await? != identity.chain { return Err(eyre!("RPC chain/genesis changed")); }
        let at = self.rpc.head(Commitment::Latest).await?;
        if self.rpc.code_hash(identity.executor, &at).await? != identity.executor_code_hash { return Err(eyre!("executor code identity changed")); }
        Ok(())
    }
    fn cancellation_headroom(&self, price: U256) -> Result<()> {
        let bumped = crate::economics::cancellation_price(price)?;
        if bumped > self.max_gas_price || crate::economics::charged_native(21_000, bumped)? > self.cancel_reserve {
            return Err(eyre!("initial price leaves no funded cancellation headroom"));
        }
        Ok(())
    }
    pub async fn reserve(&mut self, plan: SimulatedPlan) -> Result<()> {
        self.verify_identity().await?;
        if !self.is_idle() { return Err(eyre!("signer already has an active nonce")); }
        let (envelope, meta) = plan.into_parts();
        ensure_nonce_continuity(self.store.journal(), envelope.nonce)?;
        if self.rpc.nonce(self.store.journal().identity.signer, false).await? != envelope.nonce || self.rpc.nonce(self.store.journal().identity.signer, true).await? != envelope.nonce { return Err(eyre!("signer nonce changed or has untracked pending transaction")); }
        let head = self.rpc.head(Commitment::Latest).await?;
        if head.timestamp > meta.deadline { return Err(eyre!("plan expired before reservation")); }
        if envelope.native_bound()? > self.max_native_bound { return Err(eyre!("transaction exceeds native budget")); }
        self.cancellation_headroom(envelope.gas_price)?;
        let kind = AttemptKind::Liquidation(meta);
        let intent = Intent { reservation: envelope, kind, phase: Phase::ReservedUnsent, attempts: Vec::new(), reserved_at: head.timestamp };
        self.store.update_async(|journal| { journal.active = Some(intent); Ok(()) }).await
    }
    /// Unsent reservation may be discarded; already signed envelopes may never be discarded.
    pub async fn release_unsent(&mut self) -> Result<()> {
        self.store.update_async(|j| {
            if !matches!(j.active.as_ref().map(|a| &a.phase), Some(Phase::ReservedUnsent)) { return Err(eyre!("only unsigned reservation can be released")); }
            j.active = None; Ok(())
        }).await
    }
    pub async fn sign_reserved(&mut self, signer: &PrivateKeySigner) -> Result<()> {
        self.verify_identity().await?;
        if signer.address() != self.store.journal().identity.signer { return Err(eyre!("signer differs from journal identity")); }
        let intent = self.store.journal().active.as_ref().ok_or_else(|| eyre!("no reservation"))?.clone();
        if intent.phase != Phase::ReservedUnsent { return Err(eyre!("not an unsigned reservation")); }
        let head = self.rpc.head(Commitment::Latest).await?;
        if let AttemptKind::Liquidation(meta) = &intent.kind { if head.timestamp > meta.deadline { return Err(eyre!("reservation expired")); } }
        self.cancellation_headroom(intent.reservation.gas_price)?;
        self.rpc.simulate(signer.address(), &intent.reservation, &head).await?;
        self.ensure_signing()?;
        let raw = intent.reservation.sign(signer).await?;
        self.append_signed(intent.reservation, intent.kind, raw, head.timestamp).await
    }
    async fn append_signed(&mut self, envelope: UnsignedTx, kind: AttemptKind, raw: Bytes, now: u64) -> Result<()> {
        if envelope.native_bound()? > self.max_native_bound || envelope.gas_price > self.max_gas_price { return Err(eyre!("attempt native/price budget exceeded")); }
        let max_attempts = self.max_attempts;
        let created_at_unix_ms = Some(wall_time_ms()?);
        self.store.update_async(|j| {
            let attempt = SignedAttempt { hash: keccak256(&raw), raw, envelope: envelope.clone(), kind: kind.clone(), created_at: now, broadcast_at: None, created_at_unix_ms };
            validate_attempt(&j.identity, &attempt)?;
            let active = j.active.as_mut().ok_or_else(|| eyre!("no active reservation"))?;
            if active.reservation.nonce != envelope.nonce || matches!(active.phase, Phase::Mined(_) | Phase::UnknownNonceConsumed) { return Err(eyre!("cannot replace resolved/unknown nonce")); }
            let capacity = if matches!(kind, AttemptKind::Liquidation(_)) { max_attempts - 1 } else { max_attempts };
            if active.attempts.len() >= capacity { return Err(eyre!("finite attempt budget exhausted; last slot reserved for cancellation")); }
            if let Some(previous) = active.attempts.last() { if envelope.gas_price <= previous.envelope.gas_price { return Err(eyre!("replacement must increase signed fee")); } }
            active.attempts.push(attempt); active.reservation = envelope; active.kind = kind; active.phase = Phase::SignedUncertain; Ok(())
        }).await
    }
    pub async fn replace(&mut self, plan: SimulatedPlan, signer: &PrivateKeySigner) -> Result<()> {
        if !matches!(self.reconcile().await?, ReconcileOutcome::Pending | ReconcileOutcome::Expired) { return Err(eyre!("replacement requires reconciled unmined nonce")); }
        let (envelope, meta) = plan.into_parts();
        if signer.address() != self.store.journal().identity.signer { return Err(eyre!("replacement signer mismatch")); }
        let head = self.rpc.head(Commitment::Latest).await?;
        if head.timestamp > meta.deadline { return Err(eyre!("replacement plan expired")); }
        self.cancellation_headroom(envelope.gas_price)?;
        self.rpc.simulate(signer.address(), &envelope, &head).await?;
        self.ensure_signing()?;
        let raw = envelope.sign(signer).await?;
        self.append_signed(envelope, AttemptKind::Liquidation(meta), raw, head.timestamp).await
    }
    pub async fn cancel(&mut self, gas_price: U256, signer: &PrivateKeySigner) -> Result<()> {
        self.verify_identity().await?;
        if !matches!(self.reconcile().await?, ReconcileOutcome::Pending | ReconcileOutcome::Expired | ReconcileOutcome::CancelReplacementDue) { return Err(eyre!("cancel requires reconciled signed pending nonce")); }
        if signer.address() != self.store.journal().identity.signer { return Err(eyre!("cancel signer mismatch")); }
        let active = self.store.journal().active.as_ref().ok_or_else(|| eyre!("no active nonce"))?;
        if active.attempts.len() >= self.max_attempts { return Err(eyre!("finite cancellation attempt budget exhausted")); }
        let previous_price = active.attempts.last().ok_or_else(|| eyre!("no signed attempt to cancel"))?.envelope.gas_price;
        if gas_price < self.next_cancel_price(previous_price)? || gas_price > self.max_gas_price || u128::try_from(gas_price).is_err() {
            return Err(eyre!("cancellation replacement gas price exceeds policy"));
        }
        let nonce = active.reservation.nonce;
        let envelope = UnsignedTx { chain_id: self.store.journal().identity.chain.chain_id, nonce, to: signer.address(), input: Bytes::new(), gas_limit: 21_000, gas_price };
        if envelope.native_bound()? > self.max_native_bound || envelope.native_bound()? > self.cancel_reserve
            || self.rpc.balance(signer.address()).await? < envelope.native_bound()?
        { return Err(eyre!("cancellation reserve insufficient")); }
        self.ensure_signing()?;
        let raw = envelope.sign(signer).await?;
        let now = self.rpc.head(Commitment::Latest).await?.timestamp;
        self.append_signed(envelope, AttemptKind::Cancel, raw, now).await
    }
    /// Only a typed cancellation can be sent without a native-cost quote.
    pub async fn broadcast(&mut self) -> Result<()> {
        self.broadcast_inner(None).await
    }
    /// Caller supplies a quote from its trusted native-cost source. The guard is
    /// checked again after all expensive RPC reads; it never changes signed bytes.
    pub async fn broadcast_with_cost(&mut self, cost: &CostQuote, additional_profit: U256) -> Result<()> {
        self.broadcast_inner(Some((cost, additional_profit))).await
    }
    fn validate_send_guard(attempt: &SignedAttempt, at: &BlockRef, cost: Option<(&CostQuote, U256)>) -> Result<()> {
        match &attempt.kind {
            AttemptKind::Liquidation(meta) => {
                if at.timestamp > meta.deadline || at.number > meta.max_valid_block
                    || at.number < meta.as_of.number || at.timestamp < meta.as_of.timestamp
                { return Err(eyre!("signed liquidation expired or head precedes its anchor")); }
                let (cost, additional_profit) = cost.ok_or_else(|| eyre!("liquidation requires a current native cost quote"))?;
                cost.validate(meta.asset, attempt.envelope.native_bound()?, at)?;
                let required = cost.cost_asset.checked_add(additional_profit).ok_or_else(|| eyre!("current cost guard overflow"))?;
                if meta.min_asset_profit < required { return Err(eyre!("saved profit guard does not cover current native cost and allowance")); }
            }
            AttemptKind::Cancel if cost.is_some() => return Err(eyre!("native cost guard requires a liquidation attempt")),
            AttemptKind::Cancel => {},
        }
        Ok(())
    }
    async fn verify_anchor(&self, expected: &BlockRef) -> Result<()> {
        let current = self.rpc.block(expected.number).await?.ok_or_else(|| eyre!("canonical RPC anchor unavailable"))?;
        if current.number != expected.number || current.hash != expected.hash
            || current.parent_hash != expected.parent_hash || current.timestamp != expected.timestamp
        { return Err(eyre!("canonical RPC anchor changed")); }
        Ok(())
    }
    async fn broadcast_inner(&mut self, cost: Option<(&CostQuote, U256)>) -> Result<()> {
        self.verify_identity().await?;
        let status = self.reconcile().await?;
        if !matches!(status, ReconcileOutcome::Pending | ReconcileOutcome::CancelReplacementDue) { return Err(eyre!("broadcast disallowed after reconciliation: {status:?}")); }
        let attempt = self.store.journal().active.as_ref().and_then(|a| a.attempts.last()).ok_or_else(|| eyre!("no signed attempt"))?.clone();
        if status == ReconcileOutcome::CancelReplacementDue && !matches!(attempt.kind, AttemptKind::Cancel) {
            return Err(eyre!("due retry permits only the recorded typed cancellation"));
        }
        if attempt.envelope.native_bound()? > self.max_native_bound || attempt.envelope.gas_price > self.max_gas_price { return Err(eyre!("saved attempt exceeds current native/price budget")); }
        let now = self.rpc.head(Commitment::Latest).await?;
        Self::validate_send_guard(&attempt, &now, cost)?;
        let reserve = if matches!(attempt.kind, AttemptKind::Cancel) { U256::ZERO } else { self.cancel_reserve };
        let needed = attempt.envelope.native_bound()?.checked_add(reserve).ok_or_else(|| eyre!("funding overflow"))?;
        if self.rpc.balance(self.store.journal().identity.signer).await? < needed { return Err(eyre!("native funding and cleanup reserve insufficient")); }
        self.rpc.simulate(self.store.journal().identity.signer, &attempt.envelope, &now).await?;
        self.verify_anchor(&now).await?;
        if let Some((cost, _)) = cost { self.verify_anchor(&cost.validity.as_of).await?; }
        // This is the last read before sending. A pinned simulation can succeed
        // after wall-clock/block progress has already expired the immutable plan.
        let fresh = self.rpc.head(Commitment::Latest).await?;
        Self::validate_send_guard(&attempt, &fresh, cost)?;
        // Raw bytes are retained in RAM. A timeout leaves SignedUncertain intact.
        let hash = self.rpc.send_attempt(&attempt).await?;
        if hash != attempt.hash { return Err(eyre!("send returned a different transaction hash")); }
        self.store.update_async(|j| {
            let active = j.active.as_mut().ok_or_else(|| eyre!("missing active intent"))?;
            let last = active.attempts.last_mut().ok_or_else(|| eyre!("missing signed attempt"))?;
            if last.hash != attempt.hash { return Err(eyre!("attempt changed during broadcast")); }
            last.broadcast_at = Some(fresh.timestamp); active.phase = Phase::Broadcast; Ok(())
        }).await
    }
    pub async fn reconcile(&mut self) -> Result<ReconcileOutcome> {
        self.verify_identity().await?;
        let Some(intent) = self.store.journal().active.clone() else { return Ok(ReconcileOutcome::Idle); };
        if intent.phase == Phase::ReservedUnsent { return Ok(ReconcileOutcome::ReservedNeedsRefresh); }
        let latest = self.rpc.head(Commitment::Latest).await?;
        let mut found = None;
        for attempt in &intent.attempts {
            if let Some(receipt) = self.rpc.receipt(attempt.hash).await? {
                if receipt.hash != attempt.hash || receipt.block_hash.is_zero() { return Err(eyre!("malformed receipt identity from RPC")); }
                let header = self.rpc.block(receipt.block_number).await?.ok_or_else(|| eyre!("receipt block header unavailable"))?;
                if header.number != receipt.block_number || header.hash.is_zero() { return Err(eyre!("malformed receipt canonical header")); }
                if header.hash == receipt.block_hash {
                    if found.is_some() { return Err(eyre!("two canonical inclusions for one nonce")); }
                    found = Some((attempt.clone(), receipt, header));
                }
            }
        }
        if let Some((attempt, receipt, inclusion)) = found {
            // Validate the exact recorded winner and billing before changing any
            // in-memory phase, including a not-yet-finalized inclusion.
            let outcome = audit_outcome(&self.store.journal().identity, &attempt, &receipt, intent.attempts.clone(), None)?;
            let finalized = self.rpc.head(Commitment::Finalized).await?;
            if finalized.commitment != Commitment::Finalized || finalized.hash.is_zero() { return Err(eyre!("malformed finalized RPC anchor")); }
            if finalized.number >= receipt.block_number {
                if finalized.timestamp < inclusion.timestamp
                    || finalized.number == inclusion.number && (finalized.hash != inclusion.hash
                        || finalized.parent_hash != inclusion.parent_hash || finalized.timestamp != inclusion.timestamp)
                { return Err(eyre!("finalized RPC anchor contradicts receipt inclusion")); }
                // This is a trusted-RPC canonical-chain attestation, not a
                // cryptographic ancestry proof. Pin both canonical headers,
                // recheck chain/code identity and receipt, and reject any observed
                // inconsistency. The number of RPC calls is independent of age.
                self.verify_identity().await?;
                self.verify_anchor(&inclusion).await?;
                self.verify_anchor(&finalized).await?;
                let confirmed_finalized = self.rpc.head(Commitment::Finalized).await?;
                if confirmed_finalized.commitment != Commitment::Finalized || confirmed_finalized.hash.is_zero()
                    || confirmed_finalized.number < finalized.number || confirmed_finalized.timestamp < finalized.timestamp
                    || confirmed_finalized.number == finalized.number && (confirmed_finalized.hash != finalized.hash
                        || confirmed_finalized.parent_hash != finalized.parent_hash || confirmed_finalized.timestamp != finalized.timestamp)
                { return Err(eyre!("finalized RPC attestation changed while checking")); }
                if confirmed_finalized.number != finalized.number { self.verify_anchor(&confirmed_finalized).await?; }
                if self.rpc.receipt(receipt.hash).await? != Some(receipt.clone()) { return Err(eyre!("canonical receipt changed while checking")); }
                let kind = outcome.kind.clone();
                self.store.update_async(|j| { j.outcomes.push(outcome); j.active = None; Ok(()) }).await?;
                return Ok(ReconcileOutcome::Finalized(kind));
            }
            self.store.update_async(|j| { j.active.as_mut().ok_or_else(|| eyre!("missing active"))?.phase = Phase::Mined(receipt); Ok(()) }).await?;
            return Ok(ReconcileOutcome::MinedUnfinalized);
        }
        let nonce = self.rpc.nonce(self.store.journal().identity.signer, false).await?;
        if nonce > intent.reservation.nonce {
            self.store.update_async(|j| { j.active.as_mut().ok_or_else(|| eyre!("missing active"))?.phase = Phase::UnknownNonceConsumed; Ok(()) }).await?;
            return Ok(ReconcileOutcome::UnknownNonceConsumed);
        }
        if nonce < intent.reservation.nonce { return Err(eyre!("signer nonce behind reservation")); }
        if matches!(intent.phase, Phase::Mined(_) | Phase::UnknownNonceConsumed) {
            self.store.update_async(|j| { j.active.as_mut().ok_or_else(|| eyre!("missing active"))?.phase = Phase::SignedUncertain; Ok(()) }).await?;
        }
        if let AttemptKind::Liquidation(meta) = &intent.kind { if latest.timestamp > meta.deadline || latest.number > meta.max_valid_block { return Ok(ReconcileOutcome::Expired); } }
        if let Some(last) = intent.attempts.last().filter(|last| matches!(last.kind, AttemptKind::Cancel)) {
            if last.created_at_unix_ms.is_none() {
                // A caller-supplied cancellation may lack an observation time.
                // Set it once within this session without changing signed bytes.
                let now = wall_time_ms()?;
                self.store.update_async(|j| {
                    let current = j.active.as_mut().and_then(|a| a.attempts.last_mut()).ok_or_else(|| eyre!("missing cancellation during timer adoption"))?;
                    if current.hash != last.hash || current.created_at_unix_ms.is_some() { return Err(eyre!("cancellation changed during timer adoption")); }
                    current.created_at_unix_ms = Some(now); Ok(())
                }).await?;
            }
            if self.cancellation_retry_due(wall_time_ms()?)? { return Ok(ReconcileOutcome::CancelReplacementDue); }
        }
        Ok(ReconcileOutcome::Pending)
    }
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use alloy::{primitives::{Address, B256}, sol_types::SolCall};
    use barterlend_state::{ChainIdentity, PositionKey};
    use crate::executor_abi::{LiquidationExecutor, Request};

    #[tokio::test]
    async fn session_nonce_cannot_jump_over_observed_terminal_history() {
        let signer: PrivateKeySigner = "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();
        let identity = JournalIdentity { chain: ChainIdentity { chain_id: 31337, genesis_hash: B256::repeat_byte(1) },
            signer: signer.address(), executor: Address::repeat_byte(4), executor_code_hash: B256::repeat_byte(5) };
        let mut journal = Journal::new(identity.clone());
        ensure_nonce_continuity(&journal, 25).unwrap(); // A new operator ledger can follow earlier deploy/setup transactions.
        let envelope = UnsignedTx { chain_id: 31337, nonce: 7, to: signer.address(), input: Bytes::new(), gas_limit: 21_000, gas_price: U256::from(2) };
        let raw = envelope.sign(&signer).await.unwrap();
        let attempt = SignedAttempt { hash: keccak256(&raw), raw, envelope, kind: AttemptKind::Cancel,
            created_at: 101, broadcast_at: None, created_at_unix_ms: None };
        let receipt = Receipt { hash: attempt.hash, block_number: 5, block_hash: B256::repeat_byte(6),
            succeeded: true, effective_gas_price: U256::from(2), gas_used: 21_000, logs: Vec::new() };
        journal.outcomes.push(audit_outcome(&identity, &attempt, &receipt, vec![attempt.clone()], None).unwrap());
        journal.validate().unwrap();
        ensure_nonce_continuity(&journal, 8).unwrap();
        let before = journal.clone();
        assert!(ensure_nonce_continuity(&journal, 10).unwrap_err().to_string().contains("signer nonce skips session terminal history"));
        assert!(ensure_nonce_continuity(&journal, 7).is_err());
        assert_eq!(journal, before); // A matching live RPC nonce10 cannot reset the archived spend cursor7.
        journal.outcomes.clear();
        journal.archive = ArchiveSummary { outcome_count: 1, native_spent: U256::from(42_000), last_nonce: Some(7) };
        journal.validate().unwrap();
        ensure_nonce_continuity(&journal, 8).unwrap();
        assert!(ensure_nonce_continuity(&journal, 10).is_err()); // Also enforce the compact summary fallback.
        journal.archive.last_nonce = Some(u64::MAX);
        assert!(ensure_nonce_continuity(&journal, 0).is_err()); // Never wrap the session nonce cursor.
    }

    #[tokio::test]
    async fn liquidation_replacements_leave_one_signed_slot_for_typed_cancellation() {
        let signer: PrivateKeySigner = "0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();
        let identity = JournalIdentity { chain: ChainIdentity { chain_id: 31337, genesis_hash: B256::repeat_byte(1) },
            signer: signer.address(), executor: Address::repeat_byte(4), executor_code_hash: B256::repeat_byte(5) };
        let rpc = Arc::new(NoRpc);
        assert!(TxWorker::new(JournalStore::new(identity.clone()).unwrap(), rpc.clone(), 1, U256::from(1_000_000), U256::from(100_000)).is_err());
        let key = PositionKey { pool: Address::repeat_byte(2), market: 1_000, borrower: Address::repeat_byte(3) };
        let anchor = BlockRef { number: 1, hash: B256::repeat_byte(1), parent_hash: B256::ZERO, timestamp: 1, commitment: Commitment::Latest };
        let request = Request { pool: key.pool, market: key.market, borrower: key.borrower, requestedDebt: 100,
            minDiscountOut: U256::from(1), deadline: U256::from(100), path: Bytes::new(), minAssetProfit: U256::from(1), minSwapOutput: U256::from(1), planFingerprint: B256::repeat_byte(9) };
        let mut envelope = UnsignedTx { chain_id: 31337, nonce: 7, to: identity.executor,
            input: LiquidationExecutor::executeCall { request }.abi_encode().into(), gas_limit: 100_000, gas_price: U256::from(1) };
        let mut metadata = PlanMeta { key, epoch: 1, as_of: anchor, deadline: 100, max_valid_block: 100,
            requested_debt: U256::from(100), min_discount: U256::from(1), min_asset_profit: U256::from(1), min_swap_output: Some(U256::from(1)),
            plan_fingerprint: B256::repeat_byte(9), simulation_fingerprint: envelope.fingerprint().unwrap(),
            paid_state_fingerprint: B256::repeat_byte(8), asset: Address::repeat_byte(6) };
        let mut store = JournalStore::new(identity).unwrap();
        store.update(|journal| { journal.active = Some(Intent { reservation: envelope.clone(), kind: AttemptKind::Liquidation(metadata.clone()),
            phase: Phase::ReservedUnsent, attempts: Vec::new(), reserved_at: 1 }); Ok(()) }).unwrap();
        let mut worker = TxWorker::new(store, rpc, 3, U256::from(1_000_000), U256::from(100_000)).unwrap();
        for price in [1, 2] {
            envelope.gas_price = U256::from(price); metadata.simulation_fingerprint = envelope.fingerprint().unwrap();
            worker.append_signed(envelope.clone(), AttemptKind::Liquidation(metadata.clone()), envelope.sign(&signer).await.unwrap(), 1).await.unwrap();
        }
        let before = worker.store.journal().clone();
        envelope.gas_price = U256::from(3); metadata.simulation_fingerprint = envelope.fingerprint().unwrap();
        assert!(worker.append_signed(envelope.clone(), AttemptKind::Liquidation(metadata), envelope.sign(&signer).await.unwrap(), 1).await.is_err());
        assert_eq!(*worker.store.journal(), before);
        let cancel = UnsignedTx { chain_id: 31337, nonce: 7, to: signer.address(), input: Bytes::new(), gas_limit: 21_000, gas_price: U256::from(3) };
        worker.append_signed(cancel.clone(), AttemptKind::Cancel, cancel.sign(&signer).await.unwrap(), 1).await.unwrap();
        let active = worker.store.journal().active.as_ref().unwrap();
        assert_eq!(active.attempts.len(), 3); assert!(matches!(active.kind, AttemptKind::Cancel));
        assert_eq!(active.attempts[..2], before.active.unwrap().attempts);
        worker.store.journal().validate().unwrap();
    }
    // The finite signing budget is independent of node behavior; this test must never query RPC.
    struct NoRpc;
    #[async_trait::async_trait]
    impl TxRpc for NoRpc {
        async fn identity(&self) -> Result<ChainIdentity> { unreachable!() }
        async fn code_hash(&self, _: Address, _: &BlockRef) -> Result<B256> { unreachable!() }
        async fn head(&self, _: Commitment) -> Result<BlockRef> { unreachable!() }
        async fn block(&self, _: u64) -> Result<Option<BlockRef>> { unreachable!() }
        async fn receipt(&self, _: B256) -> Result<Option<Receipt>> { unreachable!() }
        async fn nonce(&self, _: Address, _: bool) -> Result<u64> { unreachable!() }
        async fn balance(&self, _: Address) -> Result<U256> { unreachable!() }
        async fn gas_price(&self) -> Result<U256> { unreachable!() }
        async fn simulate(&self, _: Address, _: &UnsignedTx, _: &BlockRef) -> Result<Bytes> { unreachable!() }
        async fn estimate(&self, _: Address, _: &UnsignedTx, _: &BlockRef) -> Result<u64> { unreachable!() }
        async fn send_raw(&self, _: &Bytes) -> Result<B256> { unreachable!() }
    }
}
