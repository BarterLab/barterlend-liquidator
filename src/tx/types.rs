use alloy::{consensus::{Transaction, TxEnvelope, TxLegacy, SignableTransaction, transaction::SignerRecoverable}, eips::{Decodable2718, Encodable2718}, primitives::{Address, B256, Bytes, TxKind, U256, keccak256}, signers::{Signer, local::PrivateKeySigner}, sol_types::SolCall};
use barterlend_state::{BlockRef, ChainIdentity, PositionKey};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use crate::executor_abi::LiquidationExecutor;

pub const JOURNAL_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalIdentity {
    pub chain: ChainIdentity,
    pub signer: Address,
    pub executor: Address,
    pub executor_code_hash: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnsignedTx {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Address,
    pub input: Bytes,
    pub gas_limit: u64,
    // U256 wire representation avoids JSON uint128 precision loss.
    pub gas_price: U256,
}

impl UnsignedTx {
    pub fn native_bound(&self) -> Result<U256> {
        self.gas_price.checked_mul(U256::from(self.gas_limit)).ok_or_else(|| eyre!("fee bound overflow"))
    }
    pub fn fingerprint(&self) -> Result<B256> { Ok(keccak256(serde_json::to_vec(self)?)) }
    pub fn legacy(&self) -> Result<TxLegacy> {
        let gas_price = u128::try_from(self.gas_price).map_err(|_| eyre!("legacy gas price exceeds uint128"))?;
        if self.gas_limit < 21_000 || gas_price == 0 { return Err(eyre!("invalid fee envelope")); }
        Ok(TxLegacy { chain_id: Some(self.chain_id), nonce: self.nonce, gas_price, gas_limit: self.gas_limit, to: TxKind::Call(self.to), value: U256::ZERO, input: self.input.clone() })
    }
    pub async fn sign(&self, signer: &PrivateKeySigner) -> Result<Bytes> {
        let tx = self.legacy()?;
        let signature = signer.sign_hash(&tx.signature_hash()).await?;
        Ok(tx.into_signed(signature).encoded_2718().into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanMeta {
    pub key: PositionKey,
    pub epoch: u64,
    pub as_of: BlockRef,
    pub deadline: u64,
    pub max_valid_block: u64,
    pub requested_debt: U256,
    pub min_discount: U256,
    pub min_asset_profit: U256,
    /// Minimum received debt asset; mandatory for every new execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_swap_output: Option<U256>,
    pub plan_fingerprint: B256,
    pub simulation_fingerprint: B256,
    pub paid_state_fingerprint: B256,
    pub asset: Address,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttemptKind { Liquidation(PlanMeta), Cancel }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedAttempt {
    pub kind: AttemptKind,
    pub envelope: UnsignedTx,
    pub raw: Bytes,
    pub hash: B256,
    pub created_at: u64,
    pub broadcast_at: Option<u64>,
    /// Wall-clock retry origin retained in RAM. A rebroadcast must never renew
    /// an existing timer. This process does not recover attempts after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub succeeded: bool,
    pub effective_gas_price: U256,
    pub gas_used: u64,
    pub logs: Vec<ReceiptLog>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptLog { pub address: Address, pub topics: Vec<B256>, pub data: Bytes }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase { ReservedUnsent, SignedUncertain, Broadcast, Mined(Receipt), UnknownNonceConsumed }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Intent {
    pub reservation: UnsignedTx,
    pub kind: AttemptKind,
    pub phase: Phase,
    pub attempts: Vec<SignedAttempt>,
    pub reserved_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutcomeKind { Liquidated, Reverted, Cancelled, InvalidSuccessEvent }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outcome {
    pub identity: JournalIdentity,
    pub attempt: SignedAttempt,
    pub receipt: Receipt,
    pub kind: OutcomeKind,
    pub native_charge: U256,
    pub asset_gain: Option<U256>,
    pub net_asset: Option<U256>,
    // All potential same-nonce envelopes remain evidence after terminal inclusion.
    pub attempts: Vec<SignedAttempt>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub schema: u32,
    pub identity: JournalIdentity,
    pub active: Option<Intent>,
    pub outcomes: Vec<Outcome>,
    /// Process-lifetime totals for outcomes removed from the bounded memory window.
    #[serde(default, skip_serializing_if = "ArchiveSummary::is_empty")]
    pub archive: ArchiveSummary,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveSummary {
    pub outcome_count: u64,
    pub native_spent: U256,
    pub last_nonce: Option<u64>,
}

impl ArchiveSummary {
    pub fn is_empty(&self) -> bool {
        self.outcome_count == 0 && self.native_spent == U256::ZERO && self.last_nonce.is_none()
    }
}

pub fn validate_unsigned(identity: &JournalIdentity, tx: &UnsignedTx, kind: &AttemptKind) -> Result<()> {
    tx.legacy()?;
    if tx.chain_id != identity.chain.chain_id { return Err(eyre!("envelope chain differs from journal")); }
    match kind {
        AttemptKind::Cancel => {
            if tx.to != identity.signer || !tx.input.is_empty() || tx.gas_limit != 21_000 { return Err(eyre!("invalid typed cancellation")); }
        }
        AttemptKind::Liquidation(meta) => {
            if tx.to != identity.executor || tx.fingerprint()? != meta.simulation_fingerprint { return Err(eyre!("envelope differs from exact simulation")); }
            let minimum = meta.min_swap_output.ok_or_else(|| eyre!("swap output guard missing"))?;
            let call = LiquidationExecutor::executeCall::abi_decode(&tx.input)?;
            if call.abi_encode().as_slice() != tx.input.as_ref() || minimum.is_zero() || call.request.minSwapOutput != minimum {
                return Err(eyre!("noncanonical executor calldata or swap guard mismatch"));
            }
            let req = call.request;
            if req.pool != meta.key.pool || req.market != meta.key.market || req.borrower != meta.key.borrower
                || U256::from(req.requestedDebt) != meta.requested_debt || req.requestedDebt == 0 || req.requestedDebt == u128::MAX
                || req.minDiscountOut != meta.min_discount || req.deadline != U256::from(meta.deadline)
                || req.minAssetProfit != meta.min_asset_profit || req.planFingerprint != meta.plan_fingerprint
                || meta.deadline < meta.as_of.timestamp || meta.asset == Address::ZERO || meta.min_asset_profit == U256::ZERO
                || meta.max_valid_block < meta.as_of.number
                || meta.key.pool == Address::ZERO || meta.key.borrower == Address::ZERO || meta.epoch == 0
                || meta.as_of.hash == B256::ZERO || meta.plan_fingerprint == B256::ZERO || meta.paid_state_fingerprint == B256::ZERO
            { return Err(eyre!("executor calldata metadata mismatch")); }
        }
    }
    Ok(())
}

pub fn validate_attempt(identity: &JournalIdentity, attempt: &SignedAttempt) -> Result<()> {
    validate_unsigned(identity, &attempt.envelope, &attempt.kind)?;
    if attempt.created_at_unix_ms == Some(0) { return Err(eyre!("invalid signed attempt wall-clock timestamp")); }
    if keccak256(&attempt.raw) != attempt.hash { return Err(eyre!("raw transaction hash mismatch")); }
    let mut bytes = attempt.raw.as_ref();
    let decoded = TxEnvelope::decode_2718(&mut bytes)?;
    if !bytes.is_empty() || !matches!(decoded, TxEnvelope::Legacy(_)) { return Err(eyre!("only canonical legacy envelope accepted")); }
    if decoded.encoded_2718().as_slice() != attempt.raw.as_ref() || decoded.recover_signer()? != identity.signer
        || decoded.chain_id() != Some(attempt.envelope.chain_id) || decoded.nonce() != attempt.envelope.nonce
        || decoded.to() != Some(attempt.envelope.to) || decoded.value() != U256::ZERO || decoded.input() != &attempt.envelope.input
        || decoded.gas_limit() != attempt.envelope.gas_limit || decoded.gas_price() != Some(u128::try_from(attempt.envelope.gas_price).map_err(|_| eyre!("gas price overflow"))?)
    { return Err(eyre!("signed envelope identity/fields mismatch")); }
    Ok(())
}

/// Every signed replacement shares one nonce and preserves its chronological fee order.
pub fn validate_attempt_history(identity: &JournalIdentity, attempts: &[SignedAttempt]) -> Result<()> {
    if attempts.is_empty() || attempts.len() > 16 { return Err(eyre!("invalid signed attempt history capacity")); }
    let nonce = attempts[0].envelope.nonce;
    let mut hashes = std::collections::BTreeSet::new();
    let mut previous_price = None;
    for attempt in attempts {
        validate_attempt(identity, attempt)?;
        if attempt.envelope.nonce != nonce || !hashes.insert(attempt.hash)
            || previous_price.is_some_and(|price| attempt.envelope.gas_price <= price)
        { return Err(eyre!("conflicting nonce/duplicate or unordered attempt")); }
        previous_price = Some(attempt.envelope.gas_price);
    }
    Ok(())
}

impl Journal {
    pub fn new(identity: JournalIdentity) -> Self { Self { schema: JOURNAL_SCHEMA, identity, active: None, outcomes: Vec::new(), archive: ArchiveSummary::default() } }
    pub fn native_spent(&self) -> Result<U256> {
        self.outcomes.iter().try_fold(self.archive.native_spent, |sum, outcome|
            sum.checked_add(outcome.native_charge).ok_or_else(|| eyre!("native spend accounting overflow")))
    }
    pub fn outcome_count(&self) -> Result<u64> {
        self.archive.outcome_count.checked_add(u64::try_from(self.outcomes.len())?)
            .ok_or_else(|| eyre!("outcome count overflow"))
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != JOURNAL_SCHEMA || self.identity.signer == Address::ZERO || self.identity.executor == Address::ZERO
            || self.identity.executor_code_hash == B256::ZERO || self.identity.chain.genesis_hash == B256::ZERO
        { return Err(eyre!("invalid journal schema/identity")); }
        if self.outcomes.len() > 10_000 { return Err(eyre!("outcome archive capacity reached; rotate explicitly")); }
        if (self.archive.outcome_count == 0) != self.archive.last_nonce.is_none()
            || (self.archive.outcome_count == 0) != (self.archive.native_spent == U256::ZERO)
        { return Err(eyre!("invalid compact archive summary")); }
        self.native_spent()?;
        self.outcome_count()?;
        if let Some(intent) = &self.active {
            validate_unsigned(&self.identity, &intent.reservation, &intent.kind)?;
            if !intent.attempts.is_empty() { validate_attempt_history(&self.identity, &intent.attempts)?; }
            if matches!(intent.phase, Phase::ReservedUnsent) != intent.attempts.is_empty() { return Err(eyre!("reservation phase disagrees with signed attempts")); }
            if let Some(last) = intent.attempts.last() {
                if last.envelope != intent.reservation || last.kind != intent.kind { return Err(eyre!("active reservation differs from last signed attempt")); }
            }
            if let Phase::Mined(receipt) = &intent.phase {
                let winning = intent.attempts.iter().find(|attempt| attempt.hash == receipt.hash).ok_or_else(|| eyre!("unrecorded mined hash"))?;
                crate::audit::audit_outcome(&self.identity, winning, receipt, intent.attempts.clone(), None)?;
            }
        }
        let mut previous_nonce = self.archive.last_nonce;
        for outcome in &self.outcomes {
            if outcome.identity != self.identity { return Err(eyre!("corrupt outcome identity")); }
            let nonce = outcome.attempt.envelope.nonce;
            if previous_nonce.is_some_and(|previous| nonce <= previous) { return Err(eyre!("duplicate or unordered terminal nonce")); }
            previous_nonce = Some(nonce);
            let expected = crate::audit::audit_outcome(&self.identity, &outcome.attempt, &outcome.receipt, outcome.attempts.clone(), None)?;
            if outcome.native_charge != expected.native_charge || outcome.kind != expected.kind || outcome.asset_gain != expected.asset_gain
                || outcome.net_asset.is_some_and(|net| outcome.asset_gain.is_none_or(|gain| net > gain))
            { return Err(eyre!("corrupt outcome charge/classification/profit")); }
        }
        if self.active.as_ref().is_some_and(|active| previous_nonce.is_some_and(|nonce| active.reservation.nonce <= nonce)) {
            return Err(eyre!("active nonce is already archived"));
        }
        Ok(())
    }
}
