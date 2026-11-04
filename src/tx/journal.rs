//! Bounded transaction state for one process. Nothing is written to disk.
use std::collections::BTreeMap;
use alloy::primitives::B256;
use barterlend_state::PositionKey;
use color_eyre::eyre::{Result, eyre};
use super::{Journal, JournalIdentity, SignedAttempt, AttemptKind, OutcomeKind};

const MAX_FAILURE_KEYS: usize = 100_000;
pub struct JournalStore {
    journal: Journal,
    failures: BTreeMap<(PositionKey, B256), usize>,
    failure_capacity_exhausted: bool,
}

fn preserve_signed_prefix(before: &[SignedAttempt], after: &[SignedAttempt]) -> Result<()> {
    if after.len() < before.len() || before.iter().zip(after).any(|(old, new)|
        old.kind != new.kind || old.envelope != new.envelope || old.raw != new.raw || old.hash != new.hash || old.created_at != new.created_at
        || old.created_at_unix_ms.is_some_and(|stamp| new.created_at_unix_ms != Some(stamp))
        || (old.created_at_unix_ms.is_none() && new.created_at_unix_ms.is_some() && !matches!(old.kind, AttemptKind::Cancel)))
    { return Err(eyre!("signed transaction evidence is immutable")); }
    Ok(())
}
fn validate_transition(before: &Journal, after: &Journal) -> Result<()> {
    if before.archive != after.archive { return Err(eyre!("archive accounting is storage-owned and immutable")); }
    if !after.outcomes.starts_with(&before.outcomes) { return Err(eyre!("archived outcomes are append-only")); }
    let Some(active) = before.active.as_ref().filter(|a| !a.attempts.is_empty()) else { return Ok(()); };
    if let Some(terminal) = after.outcomes[before.outcomes.len()..].iter().find(|outcome|
        outcome.attempt.envelope.nonce == active.reservation.nonce) {
        preserve_signed_prefix(&active.attempts, &terminal.attempts)?;
    } else {
        let retained = after.active.as_ref().ok_or_else(|| eyre!("signed nonce cannot be discarded without terminal evidence"))?;
        if retained.reservation.nonce != active.reservation.nonce || retained.reserved_at != active.reserved_at {
            return Err(eyre!("active signed nonce/reservation age is immutable"));
        }
        preserve_signed_prefix(&active.attempts, &retained.attempts)?;
    }
    Ok(())
}

pub(crate) fn compact_journal(mut journal: Journal) -> Result<Journal> {
    journal.validate()?;
    // Retain the last terminal outcome/latch. Older outcomes are summarized in RAM for this process only.
    if journal.outcomes.len() > 1 {
        let archived = journal.outcomes.len() - 1;
        for outcome in &journal.outcomes[..archived] {
            journal.archive.outcome_count = journal.archive.outcome_count.checked_add(1).ok_or_else(|| eyre!("outcome count overflow"))?;
            journal.archive.native_spent = journal.archive.native_spent.checked_add(outcome.native_charge).ok_or_else(|| eyre!("native spend accounting overflow"))?;
            journal.archive.last_nonce = Some(outcome.attempt.envelope.nonce);
        }
        journal.outcomes.drain(..archived);
    }
    journal.validate()?;
    Ok(journal)
}


impl JournalStore {
    pub fn new(identity: JournalIdentity) -> Result<Self> {
        let journal = Journal::new(identity);
        journal.validate()?;
        Ok(Self { journal, failures: BTreeMap::new(), failure_capacity_exhausted: false })
    }
    pub fn journal(&self) -> &Journal { &self.journal }
    /// Validate first, then atomically replace the bounded in-memory state.
    pub fn update(&mut self, change: impl FnOnce(&mut Journal) -> Result<()>) -> Result<()> {
        let mut next = self.journal.clone();
        change(&mut next)?;
        if next == self.journal { return Ok(()); }
        if next.identity != self.journal.identity { return Err(eyre!("transaction identity is immutable")); }
        if self.journal.outcomes.iter().any(|o| o.kind == OutcomeKind::InvalidSuccessEvent) {
            return Err(eyre!("invalid success evidence requires operator review; session is frozen"));
        }
        next.validate()?;
        validate_transition(&self.journal, &next)?;
        let mut failures = Vec::new();
        for outcome in &next.outcomes[self.journal.outcomes.len()..] {
            if outcome.kind == OutcomeKind::Liquidated { continue; }
            let keys: std::collections::BTreeSet<_> = outcome.attempts.iter().filter_map(|attempt| match &attempt.kind {
                AttemptKind::Liquidation(meta) => Some((meta.key, meta.paid_state_fingerprint)),
                AttemptKind::Cancel => None,
            }).collect();
            failures.extend(keys);
        }
        let next = compact_journal(next)?;
        for (key, fingerprint) in failures {
            if let Some(count) = self.failures.get_mut(&(key, fingerprint)) { *count = count.saturating_add(1); }
            else if self.failures.len() < MAX_FAILURE_KEYS { self.failures.insert((key, fingerprint), 1); }
            else { self.failure_capacity_exhausted = true; }
        }
        self.journal = next;
        Ok(())
    }
    pub async fn update_async(&mut self, change: impl FnOnce(&mut Journal) -> Result<()>) -> Result<()> {
        self.update(change)
    }
    pub async fn paid_failure_count(&mut self, key: &PositionKey, fingerprint: B256) -> Result<usize> {
        if self.failure_capacity_exhausted { return Ok(usize::MAX); }
        Ok(self.failures.get(&(*key, fingerprint)).copied().unwrap_or(0))
    }
    /// Only prune against verified current debt state. Financial totals stay intact.
    pub fn retain_failures(&mut self, current: &std::collections::BTreeSet<PositionKey>) {
        self.failures.retain(|(key, _), _| current.contains(key));
        // Exhaustion is sticky: an unrecorded paid failure must never authorize another attempt.
    }
}
