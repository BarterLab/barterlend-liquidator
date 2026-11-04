use std::{collections::{BTreeMap, BTreeSet, VecDeque}, sync::Arc};
use alloy::primitives::{Address, B256, keccak256};
use barterlend_state::{PositionKey, RiskStatus, StateSnapshot};
use color_eyre::eyre::{Result, eyre};

#[derive(Clone, Debug)]
pub struct Candidate { pub key: PositionKey, pub epoch: u64, pub state_fingerprint: B256 }

#[derive(Clone, Debug)]
struct Failure { fingerprint: B256, attempts: u32, retry_after: u64, search_attempts: u64 }

pub const MAX_QUEUE_CAPACITY: usize = 100_000;

/// Keyed bounded round-robin. Snapshot refresh must not move queued keys to the tail.
pub struct Scheduler {
    queue: VecDeque<Candidate>,
    keys: BTreeSet<PositionKey>,
    failures: BTreeMap<PositionKey, Failure>,
    capacity: usize,
    max_attempts_per_state: u32,
    cooldown_seconds: u64,
    last_admitted: Option<PositionKey>,
    deferred: usize,
    shared_snapshots: BTreeMap<Address, Arc<StateSnapshot>>,
    shared_candidates: BTreeMap<PositionKey, Candidate>,
}

impl Scheduler {
    pub fn new(capacity: usize, max_attempts_per_state: u32, cooldown_seconds: u64) -> Result<Self> {
        if capacity == 0 || capacity > MAX_QUEUE_CAPACITY || max_attempts_per_state == 0 || cooldown_seconds == 0 || cooldown_seconds.checked_mul(16).is_none() { return Err(eyre!("invalid scheduler limits")); }
        Ok(Self { queue: VecDeque::new(), keys: BTreeSet::new(), failures: BTreeMap::new(), capacity, max_attempts_per_state, cooldown_seconds, last_admitted: None, deferred: 0, shared_snapshots:BTreeMap::new(), shared_candidates:BTreeMap::new() })
    }
    pub fn len(&self) -> usize { self.queue.len() }
    pub fn is_empty(&self) -> bool { self.queue.is_empty() }
    /// Eligible positions deferred by capacity on the last admission pass. They remain in the SDK snapshot.
    pub fn deferred_len(&self) -> usize { self.deferred }
    pub fn invalidate_pool(&mut self, pool: Address) {
        self.shared_snapshots.remove(&pool);
        self.shared_candidates.retain(|key,_|key.pool!=pool);
        self.queue.retain(|candidate| candidate.key.pool != pool);
        self.keys = self.queue.iter().map(|candidate| candidate.key).collect();
    }
    pub fn ingest(&mut self, snapshot: &StateSnapshot) -> Result<()> {
        self.ingest_batch(&[snapshot], snapshot.as_of.timestamp)
    }
    /// Admit across all current pools at once, so a large first pool cannot starve later pools.
    /// Capacity delays work; it must not stop the application or discard the authoritative SDK state.
    pub fn ingest_batch(&mut self, snapshots: &[&StateSnapshot], now: u64) -> Result<()> {
        let mut pools = BTreeSet::new();
        let mut candidates = BTreeMap::new();
        for snapshot in snapshots {
            let pool = snapshot.pool.address;
            if !pools.insert(pool) { return Err(eyre!("duplicate pool in scheduler batch")); }
            Self::collect_candidates(snapshot,&mut candidates)?;
        }
        self.admit(&pools,&candidates,now)
    }

    /// Immutable snapshots can be reused across control-loop iterations. Only a
    /// newly published pool snapshot needs eligibility/fingerprint traversal.
    pub fn ingest_shared(&mut self, snapshots:&[&Arc<StateSnapshot>], now:u64) -> Result<()> {
        let mut pools=BTreeSet::new();
        let mut updates=Vec::new();
        for snapshot in snapshots {
            let pool=snapshot.pool.address;
            if !pools.insert(pool) { return Err(eyre!("duplicate pool in scheduler batch")); }
            if self.shared_snapshots.get(&pool).is_some_and(|old|Arc::ptr_eq(old,snapshot)) { continue; }
            let mut changed=BTreeMap::new();
            Self::collect_candidates(snapshot,&mut changed)?;
            updates.push((pool,Arc::clone(snapshot),changed));
        }
        // Validate all changed pools before mutating the queue or its cache.
        for (pool,snapshot,changed) in updates {
            self.shared_candidates.retain(|key,_|key.pool!=pool);
            self.shared_candidates.extend(changed);
            self.shared_snapshots.insert(pool,snapshot);
        }
        // Drop absent pools too: a later recovery must revalidate their state.
        let absent:Vec<_>=self.shared_snapshots.keys().filter(|pool|!pools.contains(*pool)).copied().collect();
        for pool in absent { self.invalidate_pool(pool); }
        self.queue.retain(|candidate|pools.contains(&candidate.key.pool));
        let mut candidates=std::mem::take(&mut self.shared_candidates);
        candidates.retain(|key,_|pools.contains(&key.pool));
        let result=self.admit(&pools,&candidates,now);
        self.shared_candidates=candidates;
        result
    }

    fn collect_candidates(snapshot:&StateSnapshot,candidates:&mut BTreeMap<PositionKey,Candidate>) -> Result<()> {
        let pool=snapshot.pool.address;
        for (key, record) in &snapshot.positions {
                if *key != record.key || key.pool != pool { return Err(eyre!("scheduler position key mismatch")); }
                if !matches!(&record.risk, RiskStatus::Ready(risk) if risk.execution_ready) { continue; }
                let market = snapshot.markets.get(&record.key.market).ok_or_else(|| eyre!("market config missing"))?;
                // Excludes block/time and price updatedAt; tradable state changes reset the cooldown.
                let ratio = match &snapshot.oracle { barterlend_state::OracleStatus::Ready(price) => Some((price.numerator, price.denominator)), _ => None };
                let fingerprint = keccak256(serde_json::to_vec(&(snapshot.epoch, &record.amounts, &record.risk, market, &snapshot.globals, ratio))?);
                candidates.insert(*key, Candidate { key: *key, epoch: snapshot.epoch, state_fingerprint: fingerprint });
            }
        Ok(())
    }

    fn admit(&mut self,pools:&BTreeSet<Address>,candidates:&BTreeMap<PositionKey,Candidate>,now:u64) -> Result<()> {
        self.failures.retain(|key, _| !pools.contains(&key.pool) || candidates.contains_key(key));
        // Reset/ineligibility removes only unpublished candidates. Cooling keys release capacity.
        let failures = &self.failures;
        let blocked = |candidate: &Candidate| failures.get(&candidate.key).is_some_and(|failure|
            failure.fingerprint == candidate.state_fingerprint && failure.retry_after > now);
        self.queue.retain(|old| !pools.contains(&old.key.pool) || candidates.get(&old.key)
            .is_some_and(|new| new.epoch == old.epoch && !blocked(new)));
        for queued in &mut self.queue {
            if let Some(new) = candidates.get(&queued.key) { *queued = new.clone(); }
        }
        self.keys = self.queue.iter().map(|c| c.key).collect();
        let ordered: Vec<_> = candidates.values().collect();
        let start = ordered.partition_point(|candidate| self.last_admitted.is_some_and(|cursor| candidate.key <= cursor));
        self.deferred = 0;
        for candidate in ordered[start..].iter().chain(&ordered[..start]) {
            if self.keys.contains(&candidate.key) || blocked(candidate) { continue; }
            if self.queue.len() == self.capacity { self.deferred += 1; continue; }
            self.last_admitted = Some(candidate.key);
            self.keys.insert(candidate.key); self.queue.push_back((*candidate).clone());
        }
        Ok(())
    }
    pub fn pop_ready(&mut self, now: u64) -> Option<Candidate> {
        let count = self.queue.len();
        for _ in 0..count {
            let candidate = self.queue.pop_front()?;
            let blocked = self.failures.get(&candidate.key).is_some_and(|failure| failure.fingerprint == candidate.state_fingerprint && failure.retry_after > now);
            if blocked { self.queue.push_back(candidate); } else { self.keys.remove(&candidate.key); return Some(candidate); }
        }
        None
    }
    /// Rotates unsigned funding/route probes across retries, including timeouts.
    /// This is unrelated to the session paid-transaction failure budget.
    pub fn search_attempt(&self, key: &PositionKey) -> u64 {
        self.failures.get(key).map_or(0, |failure| failure.search_attempts)
    }
    /// Records the next unsigned probe before any await. Cancellation or timeout
    /// cannot repeat the same probe forever; this alone imposes no cooldown.
    pub fn begin_probe(&mut self, candidate: &Candidate) -> u64 {
        let failure=self.failures.entry(candidate.key).or_insert(Failure {
            fingerprint:candidate.state_fingerprint,attempts:0,retry_after:0,search_attempts:0,
        });
        if failure.fingerprint!=candidate.state_fingerprint {
            failure.fingerprint=candidate.state_fingerprint;failure.attempts=0;failure.retry_after=0;
        }
        let attempt=failure.search_attempts;
        failure.search_attempts=attempt.wrapping_add(1);
        attempt
    }
    /// Quote failures are retried periodically: liquidity can change without lending logs.
    /// Exhausted budgets lengthen cooldown, rather than permanently deleting the position.
    pub fn failed(&mut self, candidate: &Candidate, now: u64) -> Result<()> {
        let prior = self.failures.get(&candidate.key).filter(|f| f.fingerprint == candidate.state_fingerprint).map_or(0, |f| f.attempts);
        let attempts = prior.saturating_add(1).min(self.max_attempts_per_state);
        let factor = if attempts >= self.max_attempts_per_state { 16 } else { u64::from(attempts) };
        let retry_after = now.checked_add(self.cooldown_seconds.checked_mul(factor).ok_or_else(|| eyre!("cooldown overflow"))?).ok_or_else(|| eyre!("retry time overflow"))?;
        let search_attempts = self.search_attempt(&candidate.key);
        self.failures.insert(candidate.key, Failure { fingerprint: candidate.state_fingerprint, attempts, retry_after, search_attempts });
        Ok(())
    }
    pub fn succeeded(&mut self, key: &PositionKey) { self.failures.remove(key); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    fn candidate(i: u8) -> Candidate { Candidate { key: PositionKey { pool: Address::from([1;20]), market: 3, borrower: Address::from([i;20]) }, epoch: 1, state_fingerprint: B256::from([1;32]) } }
    #[test]
    fn failed_key_does_not_starve_other_keys_and_retries_at_chain_time() {
        let mut scheduler = Scheduler::new(2,2,10).unwrap();
        let a = candidate(1); let b = candidate(2);
        scheduler.queue.extend([a.clone(),b.clone()]);
        scheduler.failed(&a,100).unwrap();
        assert_eq!(scheduler.pop_ready(105).unwrap().key,b.key);
        assert!(scheduler.pop_ready(109).is_none());
        assert_eq!(scheduler.pop_ready(110).unwrap().key,a.key);
    }
    #[test]
    fn repeated_unavailable_routes_back_off_with_finite_state_budget() {
        let mut scheduler = Scheduler::new(1,2,10).unwrap(); let a = candidate(1);
        scheduler.failed(&a,100).unwrap(); scheduler.failed(&a,110).unwrap();
        scheduler.queue.push_back(a.clone());
        assert!(scheduler.pop_ready(269).is_none());
        assert_eq!(scheduler.pop_ready(270).unwrap().key,a.key);
        scheduler.failed(&a,270).unwrap(); assert_eq!(scheduler.failures[&a.key].attempts,2);
        let mut changed = a; changed.state_fingerprint = B256::from([2;32]); scheduler.queue.push_back(changed);
        assert!(scheduler.pop_ready(271).is_some());
    }
}
