//! Telemetry observes; it does not authorize transactions or mutate the journal.
use std::{collections::BTreeMap, fs};
use alloy::primitives::{Address, B256, Bytes, U256};
use barterlend_liquidator::{diagnostics::{Counters, Diagnostics, HealthFrame, PoolObservation, safe_reason},
    local_config::LocalManifest, routes::{QuoteValidity, RouteKind, RouteQuote, SearchQuality, encode_exact_input_path},
    tx::{AttemptKind, Intent, Journal, JournalIdentity, Outcome, OutcomeKind, Phase, PlanMeta, Receipt, SignedAttempt, UnsignedTx}};
use barterlend_state::{BlockRef, Commitment, MarketRiskConfig, OracleStatus, PoolDescriptor, PoolRiskConfig,
    PositionAmounts, PositionKey, PositionRecord, Price, StateSnapshot, abi::ABI_FINGERPRINT, risk::evaluate_risk};

fn manifest(_directory: &tempfile::TempDir) -> LocalManifest {
    let mut m: LocalManifest = serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    m.max_total_native_spend = U256::from(1_000_000); m.fees.native_reserve = U256::from(10); m.fees.max_fee_native = U256::from(100_000); m
}
fn head() -> BlockRef { BlockRef { number: 5, hash: B256::repeat_byte(5), parent_hash: B256::repeat_byte(4), timestamp: 50, commitment: Commitment::Latest } }
fn key(m: &LocalManifest) -> PositionKey { PositionKey { pool: m.pools.values().next().unwrap().spec.address, market: 1_000, borrower: Address::repeat_byte(3) } }
fn state(m: &LocalManifest) -> StateSnapshot {
    let p = m.pools.values().next().unwrap();
    let globals = PoolRiskConfig { liquidation_ltv: U256::from(500_000_000_000_000_000u128), stale_window: 60, min_debt_remaining: 10 };
    let market = MarketRiskConfig { grace_period: 10, discount_min: U256::from(200_000_000_000_000_000u128), discount_max: U256::from(200_000_000_000_000_000u128) };
    let oracle = OracleStatus::Ready(Price { numerator: U256::from(1), denominator: U256::from(1), updated_at: U256::from(50) });
    let amounts = PositionAmounts { debt: 110, collateral: 200 };
    let record = PositionRecord { key: key(m), amounts, risk: evaluate_risk(&amounts, 1_000, &globals, &market, &oracle, 50), last_changed: head() };
    StateSnapshot { identity: m.chain, spec: p.spec, pool: PoolDescriptor { address: p.spec.address, version: 5, asset: p.asset, collateral: p.collateral,
        oracle: p.oracle, code_hash: p.code_hash }, abi_fingerprint: ABI_FINGERPRINT, epoch: 1, as_of: head(), globals,
        markets: BTreeMap::from([(1_000, market)]), oracle, positions: BTreeMap::from([(record.key, record)]) }
}
fn frame<'a>(m: &LocalManifest, snapshot: Option<&'a StateSnapshot>, journal: Option<&'a Journal>) -> HealthFrame<'a> {
    HealthFrame { head_observed_at:Some(std::time::Instant::now()), head: Some(head()), pools: vec![PoolObservation { pool: key(m).pool, snapshot, verified: snapshot.is_some(), last_success_unix_ms: Some(1), last_error: None }],
        journal, native_balance: Some(U256::from(1_000_000)), native_balance_reason: None, queue_len: 1, counters: Counters::default(), stage: "running", reason: None }
}
fn journal(m: &LocalManifest) -> Journal {
    Journal::new(JournalIdentity { chain: m.chain, signer: m.operator, executor: m.executor, executor_code_hash: m.executor_code_hash })
}
fn meta(m: &LocalManifest) -> PlanMeta { PlanMeta { key: key(m), epoch: 1, as_of: head(), deadline: 60, max_valid_block: 7, requested_debt: U256::from(100),
    min_discount: U256::from(1), min_asset_profit: U256::from(1), min_swap_output: Some(U256::from(1)), plan_fingerprint: B256::repeat_byte(9), simulation_fingerprint: B256::repeat_byte(8),
    paid_state_fingerprint: B256::repeat_byte(7), asset: m.pools.values().next().unwrap().asset.address } }
fn attempt(m: &LocalManifest, cancel: bool, gas_limit: u64, gas_price: u64) -> SignedAttempt {
    // The signature validator is tested elsewhere. These inert values exercise health accounting only.
    SignedAttempt { created_at_unix_ms: None, kind: if cancel { AttemptKind::Cancel } else { AttemptKind::Liquidation(meta(m)) },
        envelope: UnsignedTx { chain_id: 31337, nonce: 7, to: m.executor, input: Bytes::new(), gas_limit, gas_price: U256::from(gas_price) },
        raw: Bytes::from(vec![1,2,3]), hash: B256::repeat_byte(4), created_at: 50, broadcast_at: None }
}
fn active(j: &mut Journal, attempts: Vec<SignedAttempt>) {
    let last = attempts.last().unwrap().clone(); j.active = Some(Intent { reservation: last.envelope, kind: last.kind, phase: Phase::SignedUncertain, attempts, reserved_at: 50 });
}
fn outcome(m: &LocalManifest, native_charge: U256) -> Outcome {
    let a = attempt(m, false, 100_000, 1);
    Outcome { identity: journal(m).identity, attempt: a.clone(), receipt: Receipt { hash: a.hash, block_number: 6, block_hash: B256::repeat_byte(6), succeeded: false,
        effective_gas_price: U256::from(1), gas_used: 20_000, logs: Vec::new() }, kind: OutcomeKind::Reverted, native_charge, asset_gain: None, net_asset: None, attempts: vec![a] }
}
fn quote(m: &LocalManifest) -> RouteQuote {
    let p = m.pools.values().next().unwrap(); let mut q = RouteQuote { token_in: p.collateral.address, token_out: p.asset.address, amount_in: U256::from(200),
        amount_out: U256::from(300), minimum_amount_out: U256::from(290), executor: m.executor, router: Some(m.v3.router),
        route: RouteKind::V3 { path: encode_exact_input_path(&[p.collateral.address,p.asset.address], &[3000]).unwrap(), tokens: vec![p.collateral.address,p.asset.address], fees: vec![3000], pools: vec![Address::repeat_byte(20)] },
        validity: QuoteValidity::at(head(), 30, 20).unwrap(), quoter_gas_estimate: U256::from(1),
        search: SearchQuality { candidates_examined: 8, quote_calls: 3, budget_exhausted: true }, fingerprint: B256::ZERO };
    q.fingerprint = q.compute_fingerprint(); q
}

#[test]
fn health_keeps_explicit_testnet_profile_and_identity_in_live_and_dry_run() {
    let dir=tempfile::tempdir().unwrap();
    for profile in ["monad-testnet","monad-testnet-proxy"] {
        let mut m=manifest(&dir);m.profile=profile.into();m.chain.chain_id=10143;m.v3.chain=m.chain;
        let s=state(&m);
        for live in [true,false] {
            let mut diagnostics=Diagnostics::new(&m,live);
            let health=diagnostics.snapshot(frame(&m,Some(&s),None));
            assert_eq!(health.profile,profile);assert_eq!(health.chain,m.chain);
            assert_eq!(health.mode,if live {"live"} else {"dry_run"});
            assert_eq!(health.pools[0].positions_total,Some(1));
        }
    }
}

#[test]
fn missing_balance_head_and_live_ledger_never_become_zero_or_current() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut d = Diagnostics::new(&m, true);
    let mut f = frame(&m, Some(&s), None); f.head = None; f.native_balance = None; f.native_balance_reason = Some("rpc_unavailable");
    let unknown = d.snapshot(f); assert_eq!(unknown.head, None); assert_eq!(unknown.native_balance.status, "unknown"); assert_eq!(unknown.native_balance.raw, None);
    assert_eq!(unknown.budget.archived_charge, None); assert_eq!(unknown.budget.remaining, None); assert!(!unknown.pools[0].actionable);
    let mut f = frame(&m, Some(&s), None); f.native_balance = Some(U256::ZERO);
    let known_zero = d.snapshot(f); assert_eq!(known_zero.native_balance.status, "known"); assert_eq!(known_zero.native_balance.raw, Some(U256::ZERO)); assert_eq!(known_zero.status, "degraded");
    let mut dry = Diagnostics::new(&m, false); let dry = dry.snapshot(frame(&m, Some(&s), None));
    assert_eq!(dry.budget.archived_charge, Some(U256::ZERO)); assert!(!dry.budget.ledger_observed);
}

#[test]
fn recovery_stage_cannot_claim_ready_and_operator_review_is_blocked_even_with_healthy_data() {
    let dir=tempfile::tempdir().unwrap();let m=manifest(&dir);let s=state(&m);let j=journal(&m);
    let mut d=Diagnostics::new(&m,true);
    assert_eq!(d.snapshot(frame(&m,Some(&s),Some(&j))).status,"ready");
    for (stage,status) in [("recovery_waiting","degraded"),("rpc_paused","degraded"),("operator_required","blocked"),("stopped","stopped")] {
        let mut f=frame(&m,Some(&s),Some(&j));f.stage=stage;f.reason=None;
        let h=d.snapshot(f);assert_eq!(h.stage,stage);assert_eq!(h.status,status);
    }
}

#[test]
fn recovery_failures_keep_exact_safe_categories_without_exporting_extra_error_payload() {
    for (message,category) in [
        ("transaction database unavailable; durable recovery required","unclassified_redacted"),
        ("transaction database signer already locked","unclassified_redacted"),
        ("transaction database state missing after binding; restore database backup","unclassified_redacted"),
        ("signer nonce skips session terminal history","nonce_history_gap"),
        ("RPC head stopped advancing","rpc_unhealthy"),
        ("mutation RPC endpoint changed during send validation","endpoint_mismatch"),
        ("finite cancellation attempt budget exhausted","attempt_budget_exhausted"),
        ("canonical RPC anchor changed","noncanonical_anchor"),
        ("signed liquidation bounds expired or changed before actual send","expired_plan"),
    ] {
        assert_eq!(safe_reason(&message),category);
        assert_eq!(safe_reason(&format!("{message} https://credential.invalid/DO_NOT_EXPORT")),"unclassified_redacted");
    }
}

#[test]
fn retained_unverified_or_orphan_snapshot_cannot_be_actionable() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut d = Diagnostics::new(&m, false);
    assert!(d.snapshot(frame(&m, Some(&s), None)).pools[0].actionable);
    let mut f = frame(&m, Some(&s), None); f.pools[0].verified = false; f.pools[0].last_error = Some("sync_failed");
    let failed = d.snapshot(f); assert_eq!(failed.pools[0].as_of, Some(s.as_of)); assert!(!failed.pools[0].verified); assert!(!failed.pools[0].actionable);
    assert_eq!(failed.pools[0].canonicality, "unknown"); assert_eq!(failed.pools[0].eligible, None);
    let mut f = frame(&m, Some(&s), None); f.head.as_mut().unwrap().hash = B256::repeat_byte(99);
    let orphan = d.snapshot(f); assert_eq!(orphan.pools[0].canonicality, "orphan_observed"); assert_eq!(orphan.pools[0].eligible, None); assert!(!orphan.pools[0].actionable);
    let mut f = frame(&m, Some(&s), None); f.head.as_mut().unwrap().number = 4; f.head.as_mut().unwrap().timestamp = 49;
    let backwards = d.snapshot(f); assert_eq!(backwards.pools[0].block_lag, None); assert_eq!(backwards.pools[0].chain_seconds_lag, None); assert!(!backwards.pools[0].actionable);
    let mut f = frame(&m, Some(&s), None); f.head.as_mut().unwrap().timestamp = 110;
    let stale = d.snapshot(f); assert_eq!(stale.pools[0].oracle_status, "stale"); assert_eq!(stale.pools[0].eligible, None); assert!(!stale.pools[0].actionable);
}

#[test]
fn pool_key_mismatch_and_missing_observation_preserve_unknown_counts() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let mut s = state(&m); let mut d = Diagnostics::new(&m, false);
    s.pool.address = Address::repeat_byte(88);
    let bad = d.snapshot(frame(&m, Some(&s), None)); assert_eq!(bad.pools[0].positions_total, None); assert_eq!(bad.pools[0].epoch, None); assert!(!bad.pools[0].verified);
    let mut f = frame(&m, None, None); f.pools.clear(); let missing = d.snapshot(f); assert_eq!(missing.pools.len(), 1); assert_eq!(missing.pools[0].pool, key(&m).pool);
}

#[test]
fn observed_route_quality_is_bounded_and_resets_with_epoch() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let mut s = state(&m); let mut d = Diagnostics::new(&m, false);
    d.snapshot(frame(&m, Some(&s), None));
    d.quoted(&key(&m), &head(), U256::from(200), &[quote(&m)]); d.validated(&key(&m), &head());
    let observed = d.snapshot(frame(&m, Some(&s), None)); assert_eq!(observed.routes.len(), 1);
    assert_eq!(observed.routes[0].successful_quotes, Some(1)); assert_eq!(observed.routes[0].search.as_ref().unwrap().quote_calls, 3);
    assert!(observed.routes[0].search.as_ref().unwrap().budget_exhausted); assert_eq!(observed.routes[0].validated_at, Some(head()));
    let other = PositionKey { pool: Address::repeat_byte(77), ..key(&m) }; d.quote_failed(&other,&head(),U256::from(1),"route_unavailable");
    assert_eq!(d.snapshot(frame(&m, Some(&s), None)).routes.len(), 1);
    d.quote_failed(&key(&m),&head(),U256::from(200),"route_unavailable"); let failed = d.snapshot(frame(&m, Some(&s), None));
    assert_eq!(failed.routes[0].successful_quotes, None); assert!(failed.routes[0].search.is_none());
    s.epoch += 1; assert!(d.snapshot(frame(&m, Some(&s), None)).routes.is_empty());
}

#[test]
fn budgets_count_archived_plus_max_potential_charge_not_sum_or_latest_cancel() {
    let dir = tempfile::tempdir().unwrap(); let mut m = manifest(&dir); m.max_total_native_spend = U256::from(100_033);
    let s = state(&m); let mut j = journal(&m); j.outcomes.push(outcome(&m,U256::from(33)));
    active(&mut j,vec![attempt(&m,false,100_000,1),attempt(&m,true,21_000,2)]);
    let original = serde_json::to_vec(&j).unwrap(); let mut d = Diagnostics::new(&m,true);
    let equality = d.snapshot(frame(&m,Some(&s),Some(&j)));
    assert_eq!(equality.budget.active_potential_bound,Some(U256::from(100_000))); assert_eq!(equality.budget.cleanup_reserve,Some(U256::ZERO));
    assert_eq!(equality.budget.committed_total,Some(U256::from(100_033))); assert_eq!(equality.budget.remaining,Some(U256::ZERO)); assert_eq!(equality.budget.within_total,Some(true));
    assert_eq!(serde_json::to_vec(&j).unwrap(),original);
    let mut low = frame(&m,Some(&s),Some(&j)); low.native_balance = Some(U256::from(99_999));
    let low = d.snapshot(low); assert_eq!(low.native_balance.active_funding_sufficient,Some(false)); assert_eq!(low.native_balance.available_after_active,None);
    active(&mut j,vec![attempt(&m,false,100_000,1)]); let no_cancel = d.snapshot(frame(&m,Some(&s),Some(&j)));
    assert_eq!(no_cancel.budget.cleanup_reserve,Some(U256::from(10))); assert_eq!(no_cancel.budget.within_total,Some(false)); assert_eq!(no_cancel.budget.remaining,None);
}

#[test]
fn overflow_and_chain_clock_regression_are_unknown_not_fake_zero() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut j = journal(&m);
    j.outcomes.push(outcome(&m,U256::MAX)); j.outcomes.push(outcome(&m,U256::from(1))); active(&mut j,vec![attempt(&m,false,100_000,1)]);
    let mut d = Diagnostics::new(&m,true); let mut f = frame(&m,Some(&s),Some(&j)); f.head.as_mut().unwrap().timestamp = 49;
    let h = d.snapshot(f); assert_eq!(h.budget.archived_charge,None); assert_eq!(h.budget.reason.as_deref(),Some("arithmetic_overflow"));
    assert_eq!(h.tx.age_chain_seconds,None); assert_eq!(h.tx.age_wall_basis,"observed_in_this_process_not_since_submission");
    assert!(h.tx.age_wall_ms.is_some());
}

#[tokio::test]
async fn health_publication_is_in_memory_throttled_and_forced_stopped_flushes() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut d = Diagnostics::new(&m,false);
    assert!(d.due(false)); d.publish(frame(&m,Some(&s),None),false).unwrap(); assert!(!d.due(false)); assert!(d.due(true));
    assert!(d.flush(std::time::Duration::from_secs(2)).await);
    let health = d.health();
    let first = health.read().unwrap().clone().unwrap(); assert_eq!(first.schema_version,1);
    let mut f = frame(&m,Some(&s),None); f.counters.skips = 42; d.publish(f,false).unwrap(); assert_eq!(health.read().unwrap().as_ref().unwrap().counters.skips,first.counters.skips);
    let mut f = frame(&m,Some(&s),None); f.stage = "stopped"; f.head = None; f.native_balance = None; f.pools[0].verified = false;
    d.publish(f,true).unwrap(); assert!(d.flush(std::time::Duration::from_secs(2)).await); let stopped = health.read().unwrap().clone().unwrap();
    assert_eq!(stopped.status,"stopped"); assert!(stopped.head.is_none()); assert_eq!(stopped.native_balance.status,"unknown"); assert!(!stopped.pools[0].verified);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(),0);
}

#[test]
fn error_redaction_uses_exact_allowlist_and_health_never_contains_endpoint_or_raw() {
    assert_eq!(safe_reason(&"route output does not cover repayment plus signed gas bound"),"unprofitable_route");
    assert_eq!(safe_reason(&"fee budget: gas price cap exceeded"),"gas_price_cap_exceeded");
    for error in ["https://user:pass@example.invalid/rpc?apikey=topsecret", "BL_LIQUIDATOR_PRIVATE_KEY=0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefabcd",
        "route output does not cover repayment plus signed gas bound https://example.invalid/secret", "fee budget: gas price cap exceeded https://secret.invalid",
        "oops\n\u{001b}[31mKEY=secret"] { assert_eq!(safe_reason(&error),"unclassified_redacted"); }
    let dir = tempfile::tempdir().unwrap(); let mut m = manifest(&dir); m.rpc_url = "http://user:pass@127.0.0.1:8545?key=topsecret".into();
    let s = state(&m); let mut j = journal(&m); active(&mut j,vec![attempt(&m,false,100_000,1)]);
    j.active.as_mut().unwrap().attempts[0].raw = Bytes::from(vec![0xde,0xad,0xbe,0xef]);
    let mut d = Diagnostics::new(&m,true); let mut f = frame(&m,Some(&s),Some(&j)); f.reason = Some("https://secret.invalid");
    let encoded = serde_json::to_string(&d.snapshot(f)).unwrap();
    for secret in ["topsecret","user:pass","deadbeef","https://secret.invalid","PRIVATE_KEY"] { assert!(!encoded.contains(secret)); }
}

#[cfg(unix)]
#[tokio::test]
async fn existing_symlink_health_is_never_opened_or_modified() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut d = Diagnostics::new(&m,false);
    let victim = dir.path().join("untouched"); fs::write(&victim,b"keep").unwrap();
    let obsolete = dir.path().join("health.json"); symlink(&victim,&obsolete).unwrap();
    d.publish(frame(&m,Some(&s),None),true).unwrap();
    assert!(d.flush(std::time::Duration::from_secs(2)).await);
    assert_eq!(fs::read(&victim).unwrap(),b"keep");
    assert_eq!(fs::read_link(obsolete).unwrap(),victim);
    assert!(d.health().read().unwrap().is_some());
}


#[test]
fn future_oracle_timestamp_does_not_publish_current_eligibility() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let mut s = state(&m); let mut d = Diagnostics::new(&m,false);
    assert_eq!(d.snapshot(frame(&m,Some(&s),None)).pools[0].eligible,Some(1));
    if let OracleStatus::Ready(ref mut price) = s.oracle { price.updated_at = U256::from(51); }
    let health = d.snapshot(frame(&m,Some(&s),None));
    assert_eq!(health.pools[0].oracle_status,"future_timestamp"); assert_eq!(health.pools[0].eligible,None); assert!(!health.pools[0].actionable);
}

#[test]
fn mined_latest_balance_is_not_debited_twice_and_initial_quote_is_retained() {
    let dir = tempfile::tempdir().unwrap(); let m = manifest(&dir); let s = state(&m); let mut j = journal(&m);
    active(&mut j,vec![attempt(&m,false,100_000,1)]);
    j.active.as_mut().unwrap().phase = Phase::Mined(outcome(&m,U256::from(100_000)).receipt);
    let mut d = Diagnostics::new(&m,true);
    // The app may quote before its first health publication learns the snapshot epoch.
    d.quoted(&key(&m),&head(),U256::from(200),&[quote(&m)]);
    let mut f = frame(&m,Some(&s),Some(&j)); f.native_balance = Some(U256::from(5));
    let h = d.snapshot(f); assert_eq!(h.tx.phase,"mined"); assert_eq!(h.native_balance.raw,Some(U256::from(5)));
    assert_eq!(h.native_balance.available_after_active,None); assert_eq!(h.native_balance.active_funding_sufficient,None);
    assert_eq!(h.native_balance.reason.as_deref(),Some("mined_charge_may_already_be_in_balance"));
    assert_eq!(h.budget.active_potential_bound,Some(U256::from(100_000))); assert_eq!(h.routes.len(),1); assert_eq!(h.routes[0].epoch,None);
}

#[test]
fn log_metadata_preserves_unknown_state_and_cannot_be_backdated_or_releveled() {
    use barterlend_liquidator::diagnostics::{log_record, unix_ms};
    let before = unix_ms();
    let input = serde_json::json!({"event":"stopped","active_pending":null,"journal_observed":false,
        "journal_status":"unobserved","stop_reason":"startup_unobserved","timestamp_unix_ms":1,"level":"debug"});
    let record = log_record(&input,false);
    assert!(record["timestamp_unix_ms"].as_u64().unwrap() >= before);
    assert!(record["timestamp_unix_ms"].as_u64().unwrap() <= unix_ms());
    assert_eq!(record["level"],"info"); assert_eq!(record["schema_version"],1);
    assert!(record["active_pending"].is_null()); assert_eq!(record["journal_observed"],false);
    assert_eq!(record["journal_status"],"unobserved"); assert_eq!(record["stop_reason"],"startup_unobserved");
    let error = log_record(&serde_json::json!({"event":"diagnostic_error","code":"command_failed","cause":"unclassified_redacted","key":null}),true);
    assert_eq!(error["level"],"error"); assert_eq!(error["code"],"command_failed");
    assert_eq!(error["cause"],"unclassified_redacted"); assert!(error["key"].is_null());
}


#[test]
fn slow_preparation_does_not_renew_the_observation_clock() {
    let dir=tempfile::tempdir().unwrap();let m=manifest(&dir);let s=state(&m);let j=journal(&m);
    let mut d=Diagnostics::new(&m,true);
    let mut f=frame(&m,Some(&s),Some(&j));
    f.head_observed_at=Some(std::time::Instant::now()-std::time::Duration::from_secs(35));
    let h=d.snapshot(f);
    assert!(h.head_age_ms.unwrap()>=35_000);
    assert!(h.pools[0].state_age_ms.unwrap()>=35_000);
    assert_eq!(h.status,"degraded");assert!(!h.pools[0].actionable);
    assert_eq!(h.pools[0].freshness,"observation_stale");
    assert_eq!(h.reason.as_deref(),Some("health_observation_stale"));
}
#[test]
fn cached_health_ages_without_rpc_and_wall_time_cannot_renew_it() {
    let dir=tempfile::tempdir().unwrap();let m=manifest(&dir);let s=state(&m);let j=journal(&m);
    let mut d=Diagnostics::new(&m,true);
    let mut h=d.snapshot(frame(&m,Some(&s),Some(&j)));
    assert_eq!(h.status,"ready");
    h.observed_unix_ms=u64::MAX;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut h=h.current();
    assert!(h.frame_age_ms>=5);assert!(h.head_age_ms.unwrap()>=5);assert!(h.pools[0].state_age_ms.unwrap()>=5);
    h.frame_age_ms=30_000;
    let aged=h.current();assert_eq!(aged.status,"degraded");assert!(!aged.pools[0].actionable);
}
#[tokio::test]
async fn http_readiness_and_metrics_report_stale_observation_without_restarting() {
    let dir=tempfile::tempdir().unwrap();let mut m=manifest(&dir);let s=state(&m);let j=journal(&m);
    let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let address=listener.local_addr().unwrap();drop(listener);
    m.health_listen=Some(address);
    let mut d=Diagnostics::new(&m,true);
    let mut f=frame(&m,Some(&s),Some(&j));f.head_observed_at=Some(std::time::Instant::now()-std::time::Duration::from_secs(35));
    d.publish(f,true).unwrap();
    let _server=barterlend_liquidator::server::start(Some(address),d.health(),std::sync::Arc::new(barterlend_liquidator::server::Progress::default())).await.unwrap();
    let client=reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(2)).build().unwrap();
    let response=client.get(format!("http://{address}/ready")).send().await.unwrap();
    assert_eq!(response.status(),503);
    let body:serde_json::Value=response.json().await.unwrap();assert!(body["head_age_ms"].as_u64().unwrap()>=35_000);
    assert_eq!(client.get(format!("http://{address}/live")).send().await.unwrap().status(),200);
    let metrics=client.get(format!("http://{address}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("barterlend_ready 0"));assert!(metrics.contains("barterlend_state_age_seconds"));
}

#[test]
fn rpc_log_details_keep_codes_but_never_provider_text_or_credentials() {
    use barterlend_liquidator::diagnostics::rpc_failure;
    use barterlend_state::Error;
    let secret = "https://user:DO_NOT_EXPORT@example.invalid/private?token=DO_NOT_EXPORT";
    for (error, reason, code) in [
        (Error::Rpc {code:429,message:secret.into()}, "rpc_response_error", Some(429)),
        (Error::Rpc {code:-32000,message:secret.into()}, "rpc_response_error", Some(-32000)),
        (Error::Transport(secret.into()), "rpc_transport_error", None),
        (Error::Decode(secret.into()), "rpc_decode_error", None),
        (Error::IdentityMismatch(secret.into()), "identity_mismatch", None),
    ] {
        let output=rpc_failure(&color_eyre::eyre::Report::from(error).wrap_err("request failed"));
        assert_eq!(output["reason"],reason);assert_eq!(output["rpc_code"],serde_json::json!(code));
        assert!(!output.to_string().contains("DO_NOT_EXPORT"));
    }
    assert_eq!(rpc_failure(&color_eyre::eyre::eyre!("RPC head stopped advancing"))["reason"],"head_not_advancing");
    assert_eq!(rpc_failure(&color_eyre::eyre::eyre!(secret))["reason"],"unclassified_redacted");
}
