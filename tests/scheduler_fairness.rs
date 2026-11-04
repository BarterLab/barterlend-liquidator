use std::collections::{BTreeMap,BTreeSet};
use alloy::primitives::{Address,U256};
use barterlend_liquidator::{local_config::LocalManifest,scheduler::Scheduler};
use barterlend_state::{*,abi::ABI_FINGERPRINT,risk::evaluate_risk};

fn snapshot(pool_id:u8, borrowers:u8)->StateSnapshot {
    let manifest:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
    let template=manifest.pools.values().next().unwrap();
    let address=Address::repeat_byte(pool_id);
    let at=BlockRef { number:5,hash:alloy::primitives::B256::repeat_byte(5),parent_hash:alloy::primitives::B256::repeat_byte(4),timestamp:50,commitment:Commitment::Latest };
    let globals=PoolRiskConfig { liquidation_ltv:U256::from(500_000_000_000_000_000u128),stale_window:60,min_debt_remaining:10 };
    let market=MarketRiskConfig { grace_period:10,discount_min:U256::from(200_000_000_000_000_000u128),discount_max:U256::from(200_000_000_000_000_000u128) };
    let oracle=OracleStatus::Ready(Price { numerator:U256::from(1),denominator:U256::from(1),updated_at:U256::from(50) });
    let positions=(1..=borrowers).map(|i| {
        let key=PositionKey { pool:address,market:1000,borrower:Address::repeat_byte(i) };
        let amounts=PositionAmounts { debt:110,collateral:200 };
        let risk=evaluate_risk(&amounts,key.market,&globals,&market,&oracle,at.timestamp);
        assert!(matches!(&risk,RiskStatus::Ready(risk) if risk.execution_ready));
        (key,PositionRecord { key,amounts,risk,last_changed:at })
    }).collect();
    StateSnapshot { identity:manifest.chain,spec:PoolSpec { address,..template.spec },
        pool:PoolDescriptor { address,version:5,asset:template.asset,collateral:template.collateral,oracle:template.oracle,code_hash:template.code_hash },
        abi_fingerprint:ABI_FINGERPRINT,epoch:1,as_of:at,globals,markets:BTreeMap::from([(1000,market)]),oracle,positions }
}

#[test]
fn capacity_one_visits_every_position_across_pools_without_stopping() {
    let first=snapshot(1,3);let second=snapshot(2,2);let mut scheduler=Scheduler::new(1,3,10).unwrap();
    let mut seen=BTreeSet::new();
    for _ in 0..5 {
        scheduler.ingest_batch(&[&first,&second],50).unwrap();
        assert_eq!(scheduler.len(),1);assert_eq!(scheduler.deferred_len(),4);
        assert!(seen.insert(scheduler.pop_ready(50).unwrap().key));
    }
    assert_eq!(seen,first.positions.keys().chain(second.positions.keys()).copied().collect());
}

#[test]
fn cooling_positions_release_capacity_and_retry_after_their_deadline() {
    let state=snapshot(1,5);let mut scheduler=Scheduler::new(2,3,10).unwrap();let mut seen=BTreeSet::new();
    for _ in 0..5 {
        scheduler.ingest_batch(&[&state],50).unwrap();
        let candidate=scheduler.pop_ready(50).unwrap();assert!(seen.insert(candidate.key));
        scheduler.failed(&candidate,50).unwrap();
    }
    scheduler.ingest_batch(&[&state],59).unwrap();assert!(scheduler.is_empty());assert_eq!(scheduler.deferred_len(),0);
    scheduler.ingest_batch(&[&state],60).unwrap();assert!(scheduler.pop_ready(60).is_some());
}

#[test]
fn epoch_reset_and_unverified_pool_invalidation_do_not_keep_old_candidates() {
    let mut first=snapshot(1,1);let second=snapshot(2,1);let mut scheduler=Scheduler::new(2,3,10).unwrap();
    scheduler.ingest_batch(&[&first,&second],50).unwrap();first.epoch+=1;
    scheduler.ingest_batch(&[&first,&second],50).unwrap();
    let candidates=[scheduler.pop_ready(50).unwrap(),scheduler.pop_ready(50).unwrap()];
    assert_eq!(candidates.iter().find(|c|c.key.pool==first.pool.address).unwrap().epoch,2);
    scheduler.ingest_batch(&[&first,&second],50).unwrap();scheduler.invalidate_pool(first.pool.address);
    assert_eq!(scheduler.len(),1);assert_eq!(scheduler.pop_ready(50).unwrap().key.pool,second.pool.address);
}

#[test]
fn malformed_batch_cannot_partially_replace_the_existing_queue() {
    let original=snapshot(1,1);let mut invalid=snapshot(2,1);let mut scheduler=Scheduler::new(1,3,10).unwrap();
    scheduler.ingest(&original).unwrap();invalid.markets.clear();
    assert!(scheduler.ingest_batch(&[&original,&invalid],50).is_err());
    assert_eq!(scheduler.pop_ready(50).unwrap().key,*original.positions.keys().next().unwrap());
}

#[test]
fn timed_out_unsigned_probes_advance_without_starving_another_pool() {
    let first=snapshot(1,1);let second=snapshot(2,1);
    let mut scheduler=Scheduler::new(1,3,1).unwrap();let mut attempts=BTreeMap::new();
    for turn in 0..16u64 {
        let now=50+turn*20;
        scheduler.ingest_batch(&[&first,&second],now).unwrap();
        let candidate=scheduler.pop_ready(now).unwrap();
        let expected=attempts.entry(candidate.key).or_insert(0u64);
        assert_eq!(scheduler.search_attempt(&candidate.key),*expected);
        assert_eq!(scheduler.begin_probe(&candidate),*expected);*expected+=1;
        // run_loop calls failed after its unsigned timeout as well as quote errors;
        // progress was already recorded before the cancellable work began.
        scheduler.failed(&candidate,now).unwrap();
        assert_eq!(scheduler.search_attempt(&candidate.key),*expected);
        // Temporarily unavailable SDK observations must not erase search progress.
        scheduler.invalidate_pool(candidate.key.pool);
    }
    assert_eq!(attempts.len(),2);
    assert!(attempts.values().all(|attempts|*attempts==8),"one timing-out pool must not monopolize bounded admission");
}

#[test]
fn changed_economic_fingerprint_resets_cooldown_but_preserves_unsigned_search_progress() {
    let mut state=snapshot(1,1);let key=*state.positions.keys().next().unwrap();
    let mut scheduler=Scheduler::new(1,3,10).unwrap();
    scheduler.ingest_batch(&[&state],50).unwrap();let before=scheduler.pop_ready(50).unwrap();
    assert_eq!(scheduler.begin_probe(&before),0);scheduler.failed(&before,50).unwrap();
    scheduler.ingest_batch(&[&state],51).unwrap();assert!(scheduler.pop_ready(51).is_none());
    state.oracle=OracleStatus::Ready(Price { numerator:U256::from(99),denominator:U256::from(100),updated_at:U256::from(51) });
    let record=state.positions.get_mut(&key).unwrap();
    record.risk=evaluate_risk(&record.amounts,key.market,&state.globals,&state.markets[&key.market],&state.oracle,51);
    scheduler.ingest_batch(&[&state],51).unwrap();let changed=scheduler.pop_ready(51).expect("new price bypasses old cooldown");
    assert_ne!(before.state_fingerprint,changed.state_fingerprint);
    assert_eq!(scheduler.search_attempt(&key),1,"changing price must not repeat the first unsigned probe forever");
    assert_eq!(scheduler.begin_probe(&changed),1);
    scheduler.failed(&changed,51).unwrap();assert_eq!(scheduler.search_attempt(&key),2);
    scheduler.ingest_batch(&[&state],60).unwrap();assert!(scheduler.pop_ready(60).is_none());
    scheduler.ingest_batch(&[&state],61).unwrap();assert_eq!(scheduler.pop_ready(61).unwrap().key,key,
        "economic-state change resets retry count to the initial cooldown");
    scheduler.succeeded(&key);assert_eq!(scheduler.search_attempt(&key),0);
}


#[test]
fn cancelled_probe_advances_before_failure_and_does_not_impose_cooldown() {
    let state=snapshot(1,1);let key=*state.positions.keys().next().unwrap();
    let mut scheduler=Scheduler::new(1,3,10).unwrap();
    scheduler.ingest_batch(&[&state],50).unwrap();let first=scheduler.pop_ready(50).unwrap();
    assert_eq!(scheduler.begin_probe(&first),0);
    // Simulate dropping the async preparation after begin_probe, before failed.
    assert_eq!(scheduler.search_attempt(&key),1);
    scheduler.ingest_batch(&[&state],50).unwrap();let resumed=scheduler.pop_ready(50).expect("starting a probe alone must not cool down the position");
    assert_eq!(scheduler.begin_probe(&resumed),1);
    assert_eq!(scheduler.begin_probe(&resumed),2,"several bounded probes in one preparation each advance once");
    scheduler.failed(&resumed,50).unwrap();assert_eq!(scheduler.search_attempt(&key),3,"failure must not double-count a started probe");
    scheduler.ingest_batch(&[&state],59).unwrap();assert!(scheduler.pop_ready(59).is_none());
    scheduler.ingest_batch(&[&state],60).unwrap();let retry=scheduler.pop_ready(60).unwrap();
    assert_eq!(scheduler.begin_probe(&retry),3);
    scheduler.succeeded(&key);assert_eq!(scheduler.search_attempt(&key),0);
}


#[test]
fn shared_snapshots_preserve_fairness_cooldown_and_remove_unavailable_pools() {
    use std::sync::Arc;
    let first=Arc::new(snapshot(1,3));let second=Arc::new(snapshot(2,2));
    let mut scheduler=Scheduler::new(1,3,10).unwrap();let mut seen=BTreeSet::new();
    for _ in 0..5 {
        scheduler.ingest_shared(&[&first,&second],50).unwrap();
        let candidate=scheduler.pop_ready(50).unwrap();assert!(seen.insert(candidate.key));
        scheduler.failed(&candidate,50).unwrap();
    }
    scheduler.ingest_shared(&[&first,&second],59).unwrap();assert!(scheduler.is_empty());
    scheduler.ingest_shared(&[&first,&second],60).unwrap();assert!(!scheduler.is_empty());
    scheduler.ingest_shared(&[],60).unwrap();assert!(scheduler.is_empty());
    scheduler.ingest_shared(&[&second],60).unwrap();
    assert_eq!(scheduler.pop_ready(60).unwrap().key.pool,second.pool.address);
    let mut repaid=(*second).clone();repaid.positions.clear();
    scheduler.ingest_shared(&[&Arc::new(repaid)],61).unwrap();assert!(scheduler.is_empty());
}

#[test]
fn malformed_shared_batch_preserves_admission_and_new_epoch_replaces_cached_candidates() {
    use std::sync::Arc;
    let original=Arc::new(snapshot(1,1));let mut scheduler=Scheduler::new(1,3,10).unwrap();
    scheduler.ingest_shared(&[&original],50).unwrap();
    let mut invalid=snapshot(2,1);invalid.markets.clear();
    assert!(scheduler.ingest_shared(&[&original,&Arc::new(invalid)],50).is_err());
    let before=scheduler.pop_ready(50).unwrap();assert_eq!(before.epoch,1);
    let mut changed=(*original).clone();changed.epoch=2;
    scheduler.ingest_shared(&[&Arc::new(changed)],50).unwrap();
    assert_eq!(scheduler.pop_ready(50).unwrap().epoch,2);
}

#[test]
fn thousands_of_positions_churn_without_retaining_old_snapshots_or_starving_pools() {
    use std::sync::Arc;
    let mut scheduler=Scheduler::new(1000,3,10).unwrap();
    let mut previous=Vec::<std::sync::Weak<StateSnapshot>>::new();
    let mut old_key=None;
    for generation in 0..32u64 {
        let mut states=Vec::new();
        for pool in 1..=8u8 {
            let mut state=snapshot(pool,1);let template=state.positions.values().next().unwrap().clone();
            state.epoch=generation+1;state.positions.clear();
            let second=state.markets[&1000].clone();state.markets.insert(2000,second);
            for index in 0..500u64 {
                let mut record=template.clone();record.key.borrower=Address::from_word(U256::from(10_000+generation*500+index).into());
                record.key.market=if index<250 {1000}else{2000};
                record.risk=evaluate_risk(&record.amounts,record.key.market,&state.globals,&state.markets[&record.key.market],&state.oracle,50);
                state.positions.insert(record.key,record);
            }
            states.push(Arc::new(state));
        }
        let refs=states.iter().collect::<Vec<_>>();
        let mut seen=BTreeSet::new();
        for _ in 0..4 {
            scheduler.ingest_shared(&refs,50).unwrap();assert!(scheduler.len()<=1000);
            while let Some(candidate)=scheduler.pop_ready(50) {
                assert!(seen.insert(candidate.key));scheduler.begin_probe(&candidate);scheduler.failed(&candidate,50).unwrap();
            }
        }
        assert_eq!(seen.len(),4000,"every pool/market must get a turn under bounded admission");
        assert!(previous.iter().all(|weak|weak.upgrade().is_none()),"superseded snapshots must be released");
        if let Some(key)=old_key {assert_eq!(scheduler.search_attempt(&key),0,"closed historical keys must be pruned");}
        old_key=seen.first().copied();previous=states.iter().map(Arc::downgrade).collect();
    }
    scheduler.ingest_shared(&[],50).unwrap();assert!(scheduler.is_empty());
    assert!(previous.iter().all(|weak|weak.upgrade().is_none()));
}
