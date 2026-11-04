//! Observational telemetry only. It never signs, sends or mutates financial state.
use std::{collections::BTreeMap, fmt::{Display, Write as FmtWrite}, sync::{Arc, RwLock}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use alloy::primitives::{Address, B256, U256};
use barterlend_state::{BlockRef, ChainIdentity, OracleStatus, PositionKey, RiskStatus, StateSnapshot};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
pub use crate::diagnostics_io::{DeliveryStats, flush_logs};
use crate::{local_config::LocalManifest, routes::{RouteQuote, SearchQuality}, tx::{AttemptKind, Journal, Phase}};

/// Public terminal evidence without signatures, swap calldata or third-party logs.
pub fn public_outcome(outcome:&crate::tx::Outcome)->serde_json::Value {
    let attempt=|a:&crate::tx::SignedAttempt|serde_json::json!({"hash":a.hash,"kind":a.kind,
        "envelope":{"chain_id":a.envelope.chain_id,"nonce":a.envelope.nonce,"to":a.envelope.to,
            "gas_limit":a.envelope.gas_limit,"gas_price":a.envelope.gas_price,
            "input_hash":alloy::primitives::keccak256(&a.envelope.input)}});
    let r=&outcome.receipt;
    serde_json::json!({"identity":outcome.identity,"kind":outcome.kind,"attempt":attempt(&outcome.attempt),
        "attempts":outcome.attempts.iter().map(attempt).collect::<Vec<_>>(),"native_charge":outcome.native_charge,
        "asset_gain":outcome.asset_gain,"net_asset":outcome.net_asset,
        "receipt":{"hash":r.hash,"block_hash":r.block_hash,"block_number":r.block_number,"succeeded":r.succeeded,
            "effective_gas_price":r.effective_gas_price,"gas_used":r.gas_used,
            "logs":r.logs.iter().filter(|l|l.address==outcome.identity.executor).take(8).collect::<Vec<_>>()}})
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Counters { pub dry_run_plans: u64, pub finalized_liquidations: u64, pub skips: u64 }
pub struct PoolObservation<'a> {
    pub pool: Address, pub snapshot: Option<&'a StateSnapshot>, pub verified: bool,
    pub last_success_unix_ms: Option<u64>, pub last_error: Option<&'static str>,
}
pub struct HealthFrame<'a> {
    /// Captured before the head request, not after quote preparation/publication.
    pub head_observed_at: Option<Instant>,
    pub head: Option<BlockRef>, pub pools: Vec<PoolObservation<'a>>, pub journal: Option<&'a Journal>,
    pub native_balance: Option<U256>, pub native_balance_reason: Option<&'static str>,
    pub queue_len: usize, pub counters: Counters, pub stage: &'static str, pub reason: Option<&'static str>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoolHealth {
    pub pool: Address, pub epoch: Option<u64>, pub as_of: Option<BlockRef>, pub verified: bool,
    pub actionable: bool, pub canonicality: String, pub freshness: String,
    pub block_lag: Option<u64>, pub chain_seconds_lag: Option<u64>,
    /// Chain-time lag plus elapsed time since the reference head was acquired.
    #[serde(default)] pub state_age_ms: Option<u64>,
    pub last_success_unix_ms: Option<u64>, pub last_error: Option<String>,
    pub positions_total: Option<usize>, pub debt_positive: Option<usize>, pub eligible: Option<usize>,
    pub risk_unavailable: Option<usize>, pub oracle_status: String,
    pub oracle_failure: Option<String>,
    pub oracle_updated_at: Option<U256>, pub oracle_stale_window: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxHealth {
    pub phase: String, pub nonce: Option<u64>, pub last_hash: Option<B256>, pub last_kind: Option<String>,
    pub attempts: usize, pub reserved_at_chain_seconds: Option<u64>, pub age_chain_seconds: Option<u64>,
    pub age_wall_ms: Option<u64>, pub age_wall_basis: String, pub deadline: Option<u64>,
    pub max_valid_block: Option<u64>, pub expired: Option<bool>, pub archived_outcomes: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeBalanceHealth {
    pub status: String, pub raw: Option<U256>, pub head_hint: Option<BlockRef>, pub reason: Option<String>,
    pub available_after_active: Option<U256>, pub active_funding_sufficient: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BudgetHealth {
    pub ledger_observed: bool, pub archived_charge: Option<U256>, pub active_potential_bound: Option<U256>,
    pub cleanup_reserve: Option<U256>, pub configured_cleanup_reserve: U256, pub committed_total: Option<U256>, pub configured_total: U256,
    pub remaining: Option<U256>, pub within_total: Option<bool>, pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfiguredRoutes { #[serde(default)] pub v4_pool_count: usize, pub fees: Vec<u32>, pub intermediates: Vec<Address>, pub max_candidates: usize, pub max_quote_calls: usize }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteObservation {
    pub key: PositionKey, pub as_of: BlockRef, pub epoch: Option<u64>, pub amount_in: U256,
    pub outcome: String, pub successful_quotes: Option<usize>, pub search: Option<SearchQuality>,
    pub best_amount_out: Option<U256>, pub best_minimum_out: Option<U256>, pub validated_at: Option<BlockRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub schema_version: u32, pub profile: String, pub mode: String, pub chain: ChainIdentity,
    /// Publication wall time; head/frame ages are measured with a monotonic clock.
    pub observed_unix_ms: u64, pub head: Option<BlockRef>, pub status: String, pub stage: String,
    #[serde(default)] pub head_age_ms: Option<u64>,
    #[serde(default)] pub frame_age_ms: u64,
    #[serde(skip)] pub(crate) published_at: Option<Instant>,
    pub reason: Option<String>, pub pools: Vec<PoolHealth>, pub tx: TxHealth,
    pub native_balance: NativeBalanceHealth, pub budget: BudgetHealth,
    pub queue_len: usize, pub queue_capacity: usize, pub counters: Counters,
    pub configured_routes: ConfiguredRoutes, pub routes: Vec<RouteObservation>,
    #[serde(default)]
    pub delivery: DeliveryStats,
}

const MAX_HEALTH_AGE_MS: u64 = 30_000;
fn elapsed_ms(duration: Duration) -> u64 { duration.as_millis().min(u128::from(u64::MAX)) as u64 }

impl HealthSnapshot {
    /// Re-age a cached observation without pretending another chain read happened.
    pub fn current(self) -> Self { self.current_at(Instant::now()) }
    pub(crate) fn current_at(mut self, now: Instant) -> Self {
        let elapsed=self.published_at.map(|at|elapsed_ms(now.saturating_duration_since(at))).unwrap_or(u64::MAX);
        self.frame_age_ms=self.frame_age_ms.saturating_add(elapsed);
        self.head_age_ms=self.head_age_ms.map(|age|age.saturating_add(elapsed));
        for pool in &mut self.pools { pool.state_age_ms=pool.state_age_ms.map(|age|age.saturating_add(elapsed)); }
        self.published_at=Some(now);
        if self.frame_age_ms>=MAX_HEALTH_AGE_MS || self.head_age_ms.is_none_or(|age|age>=MAX_HEALTH_AGE_MS) {
            if self.status=="ready" { self.status="degraded".into();self.reason=Some("health_observation_stale".into()); }
            for pool in &mut self.pools {
                pool.actionable=false;
                if pool.freshness=="within_block_budget" { pool.freshness="observation_stale".into(); }
            }
        }
        self
    }
}

pub struct Diagnostics {
    manifest: LocalManifest, live: bool, epochs: BTreeMap<Address, u64>, routes: BTreeMap<Address, RouteObservation>,
    last_publish: Option<Instant>, last_summary: Option<Instant>, last_skip: Option<(PositionKey, &'static str, u64)>, active_observed: Option<(u64, Instant)>, health: Arc<RwLock<Option<HealthSnapshot>>>,
}
impl Diagnostics {
    pub fn new(manifest: &LocalManifest, live: bool) -> Self {
        Self { manifest: manifest.clone(), live, epochs: BTreeMap::new(), routes: BTreeMap::new(), last_publish: None, last_summary: None, last_skip: None, active_observed: None, health: Arc::new(RwLock::new(None)) }
    }
    /// Update the bounded pool catalog without resetting counters or route observations.
    pub(crate) fn update_catalog(&mut self, manifest:&LocalManifest) {
        self.manifest.pools=manifest.pools.clone();
    }
    pub fn due(&self, force: bool) -> bool { force || self.last_publish.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) }
    pub fn health(&self) -> Arc<RwLock<Option<HealthSnapshot>>> { self.health.clone() }
    pub fn with_health(mut self, health: Arc<RwLock<Option<HealthSnapshot>>>) -> Self { self.health=health;self }
    fn configured(&self, pool: Address) -> bool { self.manifest.pools.values().any(|p| p.spec.address == pool) }
    pub fn quoted(&mut self, key: &PositionKey, at: &BlockRef, amount_in: U256, quotes: &[RouteQuote]) {
        if !self.configured(key.pool) { return; }
        let pool = self.manifest.pools.values().find(|p| p.spec.address == key.pool).expect("configured pool");
        let valid = !quotes.is_empty() && quotes.iter().all(|q| q.validity.as_of == *at
            && q.validate(pool.collateral.address, pool.asset.address, amount_in, self.manifest.executor, at).is_ok());
        if !valid { self.quote_failed(key, at, amount_in, "invalid_quote_observation"); return; }
        let best = quotes.iter().max_by_key(|q| q.amount_out).expect("nonempty quote observation");
        self.routes.insert(key.pool, RouteObservation { key: *key, as_of: *at, epoch: self.epochs.get(&key.pool).copied(), amount_in,
            outcome: "quoted".into(), successful_quotes: Some(quotes.len()), search: Some(best.search.clone()),
            best_amount_out: Some(best.amount_out), best_minimum_out: Some(best.minimum_amount_out), validated_at: None });
    }
    pub fn quote_failed(&mut self, key: &PositionKey, at: &BlockRef, amount_in: U256, code: &'static str) {
        if !self.configured(key.pool) { return; }
        self.routes.insert(key.pool, RouteObservation { key: *key, as_of: *at, epoch: self.epochs.get(&key.pool).copied(), amount_in,
            outcome: safe_code(code).into(), successful_quotes: None, search: None, best_amount_out: None, best_minimum_out: None, validated_at: None });
    }
    pub(crate) fn skipped(&mut self, key: &PositionKey, reason: &'static str) {
        self.last_skip = Some((*key, safe_code(reason), unix_ms()));
    }
    pub fn validated(&mut self, key: &PositionKey, at: &BlockRef) {
        if let Some(route) = self.routes.get_mut(&key.pool).filter(|r| r.key == *key && r.as_of == *at) { route.validated_at = Some(*at); }
    }
    pub fn snapshot(&mut self, frame: HealthFrame<'_>) -> HealthSnapshot {
        let now = unix_ms();
        let sampled=Instant::now();
        let head_age_ms=frame.head.and(frame.head_observed_at).map(|at|elapsed_ms(sampled.saturating_duration_since(at)));
        let mut pools = Vec::new();
        for configured in self.manifest.pools.values() {
            let observation = frame.pools.iter().find(|p| p.pool == configured.spec.address);
            let snapshot = observation.and_then(|p| p.snapshot).filter(|s| s.pool.address == configured.spec.address
                && s.spec.address == configured.spec.address && s.identity == self.manifest.chain);
            if let Some(snapshot) = snapshot {
                let old = self.epochs.insert(configured.spec.address, snapshot.epoch);
                if old.is_some_and(|epoch| epoch != snapshot.epoch) {
                    self.routes.remove(&configured.spec.address);
                }
            }
            let verified = observation.is_some_and(|p| p.verified) && snapshot.is_some();
            let last_error = observation.and_then(|p| p.last_error).map(|code| safe_code(code).to_owned());
            let lag = snapshot.zip(frame.head).and_then(|(s, h)| h.number.checked_sub(s.as_of.number));
            let seconds = snapshot.zip(frame.head).and_then(|(s, h)| h.timestamp.checked_sub(s.as_of.timestamp));
            let orphan = snapshot.zip(frame.head).is_some_and(|(s, h)| h.number == s.as_of.number && h.hash != s.as_of.hash);
            let future = snapshot.zip(frame.head).is_some_and(|(s, h)| h.number < s.as_of.number || h.timestamp < s.as_of.timestamp);
            let freshness = if snapshot.is_none() { "unavailable" } else if orphan { "orphaned" } else if future { "head_precedes_snapshot" }
                else if !verified { "unverified" } else if frame.head.is_none() { "unknown" }
                else if lag.is_some_and(|value| value > self.manifest.v3.limits.max_age_blocks) { "stale" } else { "within_block_budget" };
            let canonicality = if orphan { "orphan_observed" } else if verified { "verified_at_last_sync" } else { "unknown" };
            let oracle_fresh = snapshot.zip(frame.head).is_some_and(|(s, h)| match &s.oracle {
                OracleStatus::Ready(p) => p.updated_at <= U256::from(h.timestamp) && p.updated_at.checked_add(U256::from(s.globals.stale_window)).is_some_and(|expiry| U256::from(h.timestamp) < expiry),
                OracleStatus::Unavailable(_) => false,
            });
            let oracle_status = match snapshot.map(|s| &s.oracle) { Some(OracleStatus::Ready(_)) if oracle_fresh => "ready",
                Some(OracleStatus::Ready(_)) if frame.head.is_none() => "freshness_unknown",
                Some(OracleStatus::Ready(p)) if frame.head.is_some_and(|h| p.updated_at > U256::from(h.timestamp)) => "future_timestamp", Some(OracleStatus::Ready(_)) => "stale",
                Some(OracleStatus::Unavailable(_)) => "unavailable", None => "unknown" };
            pools.push(PoolHealth { pool: configured.spec.address, epoch: snapshot.map(|s| s.epoch), as_of: snapshot.map(|s| s.as_of), verified,
                actionable: freshness == "within_block_budget" && oracle_fresh, canonicality: canonicality.into(), freshness: freshness.into(),
                block_lag: lag, chain_seconds_lag: seconds, state_age_ms: seconds.zip(head_age_ms).map(|(seconds,age)|seconds.saturating_mul(1000).saturating_add(age)), last_success_unix_ms: observation.and_then(|p| p.last_success_unix_ms), last_error,
                positions_total: snapshot.map(|s| s.positions.len()), debt_positive: snapshot.map(|s| s.positions.values().filter(|p| p.amounts.debt > 0).count()),
                eligible: snapshot.filter(|_| freshness == "within_block_budget" && oracle_fresh).map(|s| s.positions.values().filter(|p| matches!(&p.risk, RiskStatus::Ready(r) if r.eligible)).count()),
                risk_unavailable: snapshot.map(|s| s.positions.values().filter(|p| matches!(&p.risk, RiskStatus::Unavailable(_))).count()),
                oracle_status: oracle_status.into(), oracle_failure: snapshot.and_then(|s| match &s.oracle {
                    OracleStatus::Ready(_)=>None,
                    OracleStatus::Unavailable(reason)=>Some(match reason {
                        barterlend_state::OracleFailure::Rpc(_)=>"rpc",
                        barterlend_state::OracleFailure::Decode(_)=>"decode",
                        barterlend_state::OracleFailure::PairMismatch {..}=>"pair_mismatch",
                        barterlend_state::OracleFailure::ZeroNumerator=>"zero_numerator",
                        barterlend_state::OracleFailure::ZeroDenominator=>"zero_denominator",
                        barterlend_state::OracleFailure::RatioTooLarge=>"ratio_too_large",
                        barterlend_state::OracleFailure::TimestampOverflow=>"timestamp_overflow",
                        barterlend_state::OracleFailure::Stale {..}=>"stale",
                    }.into()),
                }), oracle_updated_at: snapshot.and_then(|s| match s.oracle { OracleStatus::Ready(p) => Some(p.updated_at), _ => None }),
                oracle_stale_window: snapshot.map(|s| s.globals.stale_window) });
        }
        let tx = self.tx_health(frame.journal, frame.head);
        let budget = self.budget_health(frame.journal);
        let required = if frame.journal.is_some_and(|j| j.active.is_none()) {
            self.manifest.fees.max_fee_native.checked_add(self.manifest.fees.native_reserve)
        } else { budget.active_potential_bound.zip(budget.cleanup_reserve).and_then(|(a, r)| a.checked_add(r)) };
        // A latest balance may already include the mined charge. Do not debit it twice.
        let mined = frame.journal.and_then(|j| j.active.as_ref()).is_some_and(|a| matches!(a.phase, Phase::Mined(_)));
        let enough = if mined { None } else { frame.native_balance.zip(required).map(|(b, r)| b >= r) };
        let native = NativeBalanceHealth { status: if frame.native_balance.is_some() { "known" } else { "unknown" }.into(), raw: frame.native_balance,
            head_hint: frame.head, reason: frame.native_balance_reason.map(|r| safe_code(r).into()).or_else(||
                (mined && frame.native_balance.is_some()).then(|| "mined_charge_may_already_be_in_balance".into())),
            available_after_active: if mined { None } else { frame.native_balance.zip(required).and_then(|(b, r)| b.checked_sub(r)) }, active_funding_sufficient: enough };
        let stage = safe_code(frame.stage);
        let blocked = matches!(stage, "blocked" | "operator_required") || tx.phase == "unknown_nonce_consumed" || budget.within_total == Some(false);
        let degraded = pools.is_empty() || matches!(stage, "recovery_waiting" | "rpc_paused") || frame.reason.is_some() || frame.head.is_none() || pools.iter().any(|p| !p.actionable || p.last_error.is_some())
            || self.live && (!budget.ledger_observed || budget.reason.is_some() || frame.native_balance.is_none() || enough == Some(false));
        let status = if stage == "stopped" { "stopped" } else if blocked { "blocked" } else if degraded { "degraded" } else { "ready" };
        HealthSnapshot { schema_version: 1, profile: self.manifest.profile.clone(), mode: if self.live { "live" } else { "dry_run" }.into(), chain: self.manifest.chain,
            observed_unix_ms: now, head: frame.head, head_age_ms, frame_age_ms:0, published_at:Some(sampled), status: status.into(), stage: stage.into(), reason: frame.reason.map(|r| safe_code(r).into()),
            pools, tx, native_balance: native, budget, queue_len: frame.queue_len, queue_capacity: self.manifest.queue_capacity,
            counters: frame.counters, configured_routes: ConfiguredRoutes { v4_pool_count: self.manifest.v4.as_ref().map_or(0, |v4| v4.pools.len()), fees: self.manifest.v3.fee_tiers.clone(), intermediates: self.manifest.v3.intermediates.clone(),
                max_candidates: self.manifest.v3.limits.max_candidates, max_quote_calls: self.manifest.v3.limits.max_quote_calls }, routes: self.routes.values().cloned().collect(), delivery: crate::diagnostics_io::stats() }.current_at(sampled)
    }
    fn tx_health(&mut self, journal: Option<&Journal>, head: Option<BlockRef>) -> TxHealth {
        let active = journal.and_then(|j| j.active.as_ref());
        if let Some(active) = active {
            if self.active_observed.as_ref().is_none_or(|(nonce, _)| *nonce != active.reservation.nonce) { self.active_observed = Some((active.reservation.nonce, Instant::now())); }
        } else { self.active_observed = None; }
        let last = active.and_then(|a| a.attempts.last());
        let meta = active.and_then(|a| match &a.kind { AttemptKind::Liquidation(m) => Some(m), AttemptKind::Cancel => None });
        TxHealth { phase: match active.map(|a| &a.phase) {
            Some(Phase::ReservedUnsent) => "reserved_unsent", Some(Phase::SignedUncertain) => "signed_uncertain", Some(Phase::Broadcast) => "broadcast",
            Some(Phase::Mined(_)) => "mined", Some(Phase::UnknownNonceConsumed) => "unknown_nonce_consumed", None if journal.is_some() => "idle", None if self.live => "not_observed", None => "disabled" }.into(),
            nonce: active.map(|a| a.reservation.nonce), last_hash: last.map(|a| a.hash),
            last_kind: active.map(|a| match a.kind { AttemptKind::Liquidation(_) => "liquidation", AttemptKind::Cancel => "cancel" }.into()),
            attempts: active.map_or(0, |a| a.attempts.len()), reserved_at_chain_seconds: active.map(|a| a.reserved_at),
            age_chain_seconds: active.zip(head).and_then(|(a, h)| h.timestamp.checked_sub(a.reserved_at)),
            age_wall_ms: self.active_observed.as_ref().and_then(|(_, at)| u64::try_from(at.elapsed().as_millis()).ok()),
            age_wall_basis: "observed_in_this_process_not_since_submission".into(), deadline: meta.map(|m| m.deadline), max_valid_block: meta.map(|m| m.max_valid_block),
            expired: meta.zip(head).map(|(m, h)| h.timestamp > m.deadline || h.number > m.max_valid_block), archived_outcomes: journal.and_then(|j| j.outcome_count().ok()).and_then(|n| usize::try_from(n).ok()) }
    }
    fn budget_health(&self, journal: Option<&Journal>) -> BudgetHealth {
        let archive = journal.map(|j| j.native_spent().ok()).unwrap_or(if self.live { None } else { Some(U256::ZERO) });
        let active = journal.and_then(|j| j.active.as_ref());
        let bound = match active {
            Some(a) if a.attempts.is_empty() => a.reservation.native_bound().ok(),
            Some(a) => a.attempts.iter().try_fold(U256::ZERO, |max, attempt| attempt.envelope.native_bound().ok().map(|bound| max.max(bound))),
            None if journal.is_some() || !self.live => Some(U256::ZERO), None => None,
        };
        let needs_reserve = active.is_some_and(|a| matches!(a.kind, AttemptKind::Liquidation(_)));
        let reserve = if journal.is_none() && self.live { None } else { Some(if needs_reserve { self.manifest.fees.native_reserve } else { U256::ZERO }) };
        let committed = archive.zip(bound).zip(reserve).and_then(|((a, b), r)| a.checked_add(b).and_then(|v| v.checked_add(r)));
        let reason = if journal.is_none() && self.live { Some("journal_not_observed") }
            else if committed.is_none() { Some("arithmetic_overflow") } else if committed.is_some_and(|v| v > self.manifest.max_total_native_spend) { Some("total_budget_exhausted") } else { None };
        BudgetHealth { ledger_observed: journal.is_some(), archived_charge: archive, active_potential_bound: bound, cleanup_reserve: reserve,
            configured_cleanup_reserve: self.manifest.fees.native_reserve, committed_total: committed, configured_total: self.manifest.max_total_native_spend,
            remaining: committed.and_then(|v| self.manifest.max_total_native_spend.checked_sub(v)), within_total: committed.map(|v| v <= self.manifest.max_total_native_spend), reason: reason.map(str::to_owned) }
    }
    pub fn publish(&mut self, frame: HealthFrame<'_>, force: bool) -> Result<()> {
        if !self.due(force) { return Ok(()); }
        let snapshot = self.snapshot(frame);
        let changed = self.health.read().ok().and_then(|h| h.as_ref().map(|h| h.status != snapshot.status || h.reason != snapshot.reason)).unwrap_or(true);
        if changed {
            emit_stdout(&serde_json::json!({"event":"health_changed","status":snapshot.status,"stage":snapshot.stage,"reason":snapshot.reason}));
        }
        // Reuse the health frame: no additional RPC, position scan or per-block logging.
        if self.last_summary.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            emit_stdout(&serde_json::json!({"event":"runtime_summary","mode":snapshot.mode,
                "status":snapshot.status,"stage":snapshot.stage,"reason":snapshot.reason,
                "head_block":snapshot.head.map(|h|h.number),"head_age_ms":snapshot.head_age_ms,
                "pools":snapshot.pools.len(),"actionable_pools":snapshot.pools.iter().filter(|p|p.actionable).count(),
                "queued":snapshot.queue_len,"tx_phase":snapshot.tx.phase,"tx_hash":snapshot.tx.last_hash,
                "counters":snapshot.counters,"native_balance":snapshot.native_balance.raw,
                "budget_remaining":snapshot.budget.remaining,"log_dropped":snapshot.delivery.log_dropped,
                "last_skip":self.last_skip.map(|(key,reason,at)|serde_json::json!({"key":key,"reason":reason,"observed_unix_ms":at}))}));
            for pool in &snapshot.pools {
                emit_stdout(&serde_json::json!({"event":"pool_status","pool":pool.pool,"actionable":pool.actionable,
                    "block":pool.as_of.map(|b|b.number),"block_lag":pool.block_lag,"state_age_ms":pool.state_age_ms,
                    "positions":pool.positions_total,"with_debt":pool.debt_positive,"eligible":pool.eligible.filter(|_|pool.actionable),
                    "risk_unavailable":pool.risk_unavailable,"oracle":pool.oracle_status,
                    "oracle_updated_at":pool.oracle_updated_at,"freshness":pool.freshness,"last_error":pool.last_error}));
            }
            self.last_summary = Some(Instant::now());
        }
        *self.health.write().map_err(|_| eyre!("health lock poisoned"))? = Some(snapshot);
        self.last_publish = Some(Instant::now());
        Ok(())
    }
    pub async fn flush(&self, timeout: Duration) -> bool { flush_logs(timeout).await }
}

pub fn unix_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|v| u64::try_from(v.as_millis()).ok()).unwrap_or(0) }
fn safe_code(code: &str) -> &str {
    if !code.is_empty() && code.len() <= 80 && code.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') { code } else { "unknown" }
}
struct BoundedDisplay(String);
impl FmtWrite for BoundedDisplay {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        if self.0.len().saturating_add(value.len()) > 512 { return Err(std::fmt::Error); }
        self.0.push_str(value); Ok(())
    }
}
pub fn safe_reason(error: &dyn Display) -> &'static str {
    let mut text = BoundedDisplay(String::new());
    if std::fmt::write(&mut text, format_args!("{error}")).is_err() { return "unclassified_redacted"; }
    match text.0.as_str() {
        "monad_config_network_mismatch" => "monad_config_network_mismatch",
        "monad_config_v3_deployment_mismatch" => "monad_config_v3_deployment_mismatch",
        "monad_config_v4_manager_not_configured" => "monad_config_v4_manager_not_configured",
        "monad_config_kyber_not_configured" => "monad_config_kyber_not_configured",
        "monad_config_kyber_code_mismatch" => "monad_config_kyber_code_mismatch",
        "monad_config_intermediate_not_approved" => "monad_config_intermediate_not_approved",
        "monad_config_no_approved_v4_pools" => "monad_config_no_approved_v4_pools",
        "config init requires --quoter outside Monad Mainnet" => "config_quoter_required",
        "invalid route configuration: comparison budget cannot cover all sources" => "route_comparison_budget_too_small",
        "route comparison timed out" => "route_timeout",
        "all compared plans expired" => "expired_plan",
        "no liquidatable funding candidates" => "no_liquidatable_candidates",
        "session paid failure budget exhausted for this borrower state" => "paid_state_budget_exhausted",
        "route output does not cover repayment plus signed gas bound" => "unprofitable_route",
        "fee budget: gas price cap exceeded" => "gas_price_cap_exceeded",
        "gas/profit guard did not converge within iteration budget" => "simulation_nonconvergent",
        "actual simulation amounts disagree with protocol funding model/guard" => "simulation_amount_mismatch",
        "insufficient native balance" | "native funding and cleanup reserve insufficient" | "cancellation reserve insufficient" => "insufficient_native",
        "fresh funding state expired" | "price expired before funding plan" | "signed quote expired before send" | "plan expired during final simulation"
            | "signed liquidation expired or head precedes its anchor" | "native cost expired"
            | "signed liquidation bounds expired or changed before actual send" => "expired_plan",
        "funding anchor changed" | "send validation anchor changed" | "canonical RPC anchor changed"
            | "canonical receipt changed while checking" | "finalized RPC attestation changed while checking"
            | "finalized RPC anchor contradicts receipt inclusion" => "noncanonical_anchor",
        "saved profit guard no longer covers current native cost; require fresh replacement"
            | "saved profit guard does not cover current native cost and allowance" => "native_cost_guard_insufficient",
        "saved send exceeds current total native/fee/gas policy; journal retained" | "saved attempt exceeds current native/price budget"
            | "total native spend budget exhausted; journal retained" | "cancellation total budget exhausted"
            | "attempt native/price budget exceeded" => "native_budget_exhausted",
        "cancellation replacement gas price exceeds policy" => "gas_price_cap_exceeded",
        "finite cancellation attempt budget exhausted" | "finite attempt budget exhausted; last slot reserved for cancellation" => "attempt_budget_exhausted",
        "operator has untracked pending nonce" | "signer nonce changed or has untracked pending transaction" => "untracked_nonce",
        "candidate queue capacity reached" => "queue_capacity",
        "RPC chain/genesis changed" | "executor code identity changed" | "fresh pool identity changed" => "identity_changed",
        "signer nonce skips session terminal history" => "nonce_history_gap",
        "RPC head is ahead of local clock tolerance" | "RPC head exceeds wall-clock age budget"
            | "RPC head stopped advancing" | "all configured RPC endpoints unavailable or stale"
            | "RPC health monitor unavailable" => "rpc_unhealthy",
        "liquidation requires a current native cost quote" => "native_cost_required",
        "read and mutation RPC endpoints differ" | "read and mutation RPC endpoint sets differ"
            | "mutation RPC endpoint changed during send validation" => "endpoint_mismatch",
        "local mutation transport requires chain31337" | "local mutation transport accepts only canonical chain31337 legacy transactions" => "local_chain_required",
        "manifest must be a regular file" => "invalid_manifest_path",
        "finality ancestry budget exceeded" => "finality_budget_exceeded",
        "unknown transaction consumed signer nonce; journal retained for operator" | "unknown signer nonce consumption; journal retained" => "unknown_nonce_consumed",
        "successful receipt has invalid executor event; operator review required"
            | "successful receipt has invalid executor event; outcome archived for operator" => "invalid_success_event",
        _ => "unclassified_redacted",
    }
}
/// Event emission time is separate from the chain timestamp and health observation time.
/// Metadata cannot be supplied or backdated by the caller; unknown fields are not added here.
pub fn log_record(value: &serde_json::Value, is_error: bool) -> serde_json::Value {
    let mut record = value.as_object().cloned().unwrap_or_default();
    record.insert("timestamp_unix_ms".into(), serde_json::json!(unix_ms()));
    record.insert("level".into(), serde_json::json!(if is_error { "error" } else { "info" }));
    record.entry("schema_version").or_insert(serde_json::json!(1));
    serde_json::Value::Object(record)
}
pub fn emit_stdout(value: &serde_json::Value) {
    crate::diagnostics_io::emit(value,false);
}
pub fn emit_stderr(value: &serde_json::Value) {
    crate::diagnostics_io::emit(value,true);
}
/// Expose typed failure categories and numeric codes, never provider messages or URLs.
pub fn rpc_failure(error: &color_eyre::eyre::Report) -> serde_json::Value {
    use barterlend_state::Error;
    let (reason, code) = match error.downcast_ref::<Error>() {
        Some(Error::Rpc { code, .. }) => ("rpc_response_error", Some(*code)),
        Some(Error::Transport(_)) => ("rpc_transport_error", None),
        Some(Error::Decode(_)) => ("rpc_decode_error", None),
        Some(Error::NonCanonical(_)) => ("noncanonical_anchor", None),
        Some(Error::MissingBlock(_)) => ("missing_block", None),
        Some(Error::IdentityMismatch(_)) => ("identity_mismatch", None),
        Some(Error::ResourceLimit(_)) => ("rpc_resource_limit", None),
        _ => {
            let mut text = BoundedDisplay(String::new());
            let _ = std::fmt::write(&mut text, format_args!("{error}"));
            (match text.0.as_str() {
                "RPC head is ahead of local clock tolerance" => "head_ahead_of_clock",
                "RPC head exceeds wall-clock age budget" => "head_too_old",
                "RPC head stopped advancing" => "head_not_advancing",
                "recovery endpoint identity mismatch" => "identity_mismatch",
                _ => "unclassified_redacted",
            }, None)
        },
    };
    serde_json::json!({"reason":reason,"rpc_code":code})
}
pub fn emit_error(code: &'static str, error: &dyn Display, key: Option<&PositionKey>) {
    emit_stderr(&serde_json::json!({"event": "diagnostic_error", "schema_version": 1, "code": safe_code(code), "cause": safe_reason(error), "key": key}));
}
