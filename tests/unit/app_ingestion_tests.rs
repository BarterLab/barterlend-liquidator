use super::*;
use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};
use alloy::primitives::B256;
use async_trait::async_trait;
use barterlend_state::Result as StateResult;
use crate::local_config::LocalPoolManifest;
use tokio::{sync::Notify, task::JoinHandle};

#[derive(Default)]
struct BlockedRead {
    entered: Notify,
    active: AtomicUsize,
    cancelled: AtomicUsize,
}

struct ReadGuard(Arc<BlockedRead>);
impl ReadGuard {
    fn enter(tracker: Arc<BlockedRead>) -> Self {
        tracker.active.fetch_add(1, Ordering::SeqCst);
        tracker.entered.notify_one();
        Self(tracker)
    }
}
impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}

struct IngestionReader {
    identity: ChainIdentity,
    descriptors: BTreeMap<Address, PoolDescriptor>,
    blocked_pool: Address,
    tracker: Arc<BlockedRead>,
    head_number: u64,
    log_ranges: Mutex<Vec<(u64,u64)>>,
    current_schedule: bool,
}

impl IngestionReader {
    fn block_ref(&self, number: u64, commitment: Commitment) -> BlockRef {
        BlockRef { number,
            hash: if number == 0 { self.identity.genesis_hash } else { B256::repeat_byte(10) },
            parent_hash: if number == 0 { B256::ZERO } else { self.identity.genesis_hash },
            timestamp: number * 10, commitment }
    }
}

#[async_trait]
impl StateReader for IngestionReader {
    async fn identity(&self) -> StateResult<ChainIdentity> { Ok(self.identity) }
    async fn head(&self, commitment: Commitment) -> StateResult<BlockRef> { Ok(self.block_ref(self.head_number,commitment)) }
    async fn block(&self, number: u64, commitment: Commitment) -> StateResult<Option<BlockRef>> {
        Ok((number <= self.head_number).then(|| self.block_ref(number,commitment)))
    }
    async fn describe_pool(&self, spec: &PoolSpec, _: &BlockRef) -> StateResult<PoolDescriptor> {
        if spec.address == self.blocked_pool {
            let guard = ReadGuard::enter(self.tracker.clone());
            let result = std::future::pending::<StateResult<PoolDescriptor>>().await;
            drop(guard);
            return result;
        }
        Ok(self.descriptors[&spec.address].clone())
    }
    async fn read_positions(&self, _: Address, keys: &[PositionKey], _: &BlockRef) -> StateResult<BTreeMap<PositionKey, PositionAmounts>> {
        Ok(keys.iter().map(|key| (*key,PositionAmounts::default())).collect())
    }
    async fn read_globals(&self, _: Address, _: &BlockRef) -> StateResult<PoolRiskConfig> {
        Ok(PoolRiskConfig { liquidation_ltv: U256::from(800_000_000_000_000_000u128),stale_window:60,min_debt_remaining:10 })
    }
    async fn read_market_configs(&self, _: Address, markets: &[u64], _: &BlockRef) -> StateResult<BTreeMap<u64,MarketRiskConfig>> {
        Ok(markets.iter().map(|market| (*market,MarketRiskConfig { grace_period:10,
            discount_min:U256::from(10_000_000_000_000_000u128),discount_max:U256::from(100_000_000_000_000_000u128) })).collect())
    }
    async fn read_oracle(&self, _: &PoolDescriptor, at: &BlockRef) -> StateResult<OracleStatus> {
        Ok(OracleStatus::Ready(Price { numerator:U256::from(1),denominator:U256::from(1),updated_at:U256::from(at.timestamp) }))
    }
    async fn logs(&self, _: Address, from: u64, to: u64) -> StateResult<Vec<RawLog>> {
        self.log_ranges.lock().unwrap().push((from,to));Ok(Vec::new())
    }
    async fn market_users(&self, _: Address, _: u64, _: u64, _: u64, _: &BlockRef) -> StateResult<Vec<Address>> { Ok(Vec::new()) }
    async fn active_markets(&self, _: Address, _: &BlockRef) -> StateResult<Vec<u64>> { Ok(if self.current_schedule {vec![1_000_001]}else{Vec::new()}) }
    async fn market_schedule(&self,_:Address,_:&BlockRef)->StateResult<barterlend_state::engine::current::MarketSchedule> {
        Ok(barterlend_state::engine::current::MarketSchedule {start:1,durations:[1_000_000;3],active:[1;3],extensions:[0;3]})
    }
}

struct IngestionTask(JoinHandle<Result<()>>);
impl Drop for IngestionTask {
    fn drop(&mut self) { self.0.abort(); }
}

fn fixture(include_healthy: bool) -> (tempfile::TempDir,LocalManifest,Arc<IngestionReader>,Arc<BlockedRead>) {
    let directory=tempfile::tempdir().unwrap();
    let mut manifest:LocalManifest=serde_json::from_str(include_str!("../../examples/local-manifest.json")).unwrap();
    manifest.poll_ms=100;
    let template=manifest.pools.values().next().unwrap().clone();
    manifest.pools.clear();
    let tracker=Arc::new(BlockedRead::default());
    let blocked_pool=Address::repeat_byte(1);
    let mut descriptors=BTreeMap::new();
    for (name,address) in [("a-blocked",blocked_pool),("b-healthy",Address::repeat_byte(2))] {
        if !include_healthy && address != blocked_pool { continue; }
        let pool=LocalPoolManifest { spec:PoolSpec { address,deployment_block:0,expected_version:5,
            deployment_block_hash:Some(manifest.chain.genesis_hash) },..template.clone() };
        descriptors.insert(address,PoolDescriptor { address,version:5,asset:pool.asset,collateral:pool.collateral,
            oracle:pool.oracle,code_hash:pool.code_hash });
        manifest.pools.insert(name.into(),pool);
    }
    manifest.validate().unwrap();
    let reader=Arc::new(IngestionReader { identity:manifest.chain,descriptors,blocked_pool,tracker:tracker.clone(),head_number:1,log_ranges:Mutex::new(Vec::new()),current_schedule:true });
    (directory,manifest,reader,tracker)
}

async fn finish_ingestion(task: &mut IngestionTask, tracker: &BlockedRead) {
    tokio::time::timeout(Duration::from_secs(2),&mut task.0).await
        .expect("ingestion did not cancel blocked pool reads promptly").unwrap().unwrap();
    assert_eq!(tracker.active.load(Ordering::SeqCst),0,"ingestion returned with an orphaned pool read");
    assert_eq!(tracker.cancelled.load(Ordering::SeqCst),1,"blocked read future was not dropped exactly once");
}

#[tokio::test]
async fn current_census_isolates_blocked_pools_and_never_writes_files() {
    let (directory,manifest,reader,tracker)=fixture(true);
    let (view_tx,mut view_rx)=watch::channel(BTreeMap::new());
    let (stop_tx,stop_rx)=watch::channel(false);
    let mut task=IngestionTask(tokio::spawn(ingest_pools(manifest,reader,false,view_tx,stop_rx)));
    tokio::time::timeout(Duration::from_secs(2),tracker.entered.notified()).await.unwrap();
    let healthy=Address::repeat_byte(2);
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {
            if view_rx.borrow().get(&healthy).is_some_and(|v| v.verified && v.snapshot.is_some()) {break;}
            view_rx.changed().await.unwrap();
        }
    }).await.expect("healthy pool was blocked by another pool");
    let snapshot=view_rx.borrow()[&healthy].snapshot.clone().unwrap();
    assert_eq!(snapshot.as_of.number,1);
    assert_eq!(snapshot.pool.address,healthy);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(),0);
    stop_tx.send(true).unwrap();finish_ingestion(&mut task,&tracker).await;
}

#[tokio::test]
async fn explicit_stop_and_closed_channel_both_cancel_inflight_census() {
    for closed in [false,true] {
        let (_directory,manifest,reader,tracker)=fixture(false);
        let(view_tx,_view_rx)=watch::channel(BTreeMap::new());let(stop_tx,stop_rx)=watch::channel(false);
        let mut task=IngestionTask(tokio::spawn(ingest_pools(manifest,reader,false,view_tx,stop_rx)));
        tokio::time::timeout(Duration::from_secs(2),tracker.entered.notified()).await.unwrap();
        if !closed {stop_tx.send(true).unwrap();}
        drop(stop_tx);finish_ingestion(&mut task,&tracker).await;
    }
}

#[tokio::test]
async fn cold_current_census_never_replays_history_or_reads_old_runtime_files() {
    let(directory,mut manifest,mut reader,_tracker)=fixture(true);
    manifest.pools.remove("a-blocked");Arc::get_mut(&mut reader).unwrap().head_number=5000;
    let old=directory.path().join("journal.json");std::fs::write(&old,b"old financial data must not change").unwrap();
    let spec=manifest.pools["b-healthy"].spec;
    let config=EngineConfig {commitment:Commitment::Latest,expected_identity:Some(manifest.chain),..Default::default()};
    let mut sync=crate::pool_sync::PoolSync::new(reader.clone(),spec,config,&manifest.sync).unwrap();
    assert!(sync.snapshot_shared().is_none());
    for _ in 0..32 {if matches!(sync.sync_step().await.unwrap(),SyncStepOutcome::Complete(_)){break;}}
    assert_eq!(sync.snapshot_shared().unwrap().as_of.number,5000);
    assert!(reader.log_ranges.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(&old).unwrap(),b"old financial data must not change");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(),1);
}

#[tokio::test]
async fn catalog_addition_starts_new_pool_without_restarting_existing_workers() {
    let (_directory,mut manifest,reader,tracker)=fixture(true);
    let healthy=manifest.pools.remove("b-healthy").unwrap();
    let (catalog_tx,catalog_rx)=watch::channel(manifest.clone());
    let (view_tx,mut view_rx)=watch::channel(BTreeMap::new());let(stop_tx,stop_rx)=watch::channel(false);
    let mut task=IngestionTask(tokio::spawn(ingest_catalog(catalog_rx,reader,false,view_tx,stop_rx)));
    tokio::time::timeout(Duration::from_secs(2),tracker.entered.notified()).await.unwrap();
    manifest.pools.insert("b-healthy".into(),healthy);catalog_tx.send(manifest.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {if view_rx.borrow().get(&Address::repeat_byte(2)).is_some_and(|v|v.verified){break;}view_rx.changed().await.unwrap();}
    }).await.unwrap();
    // Duplicate notifications must not launch duplicate synchronizers or cancel the blocked one.
    catalog_tx.send(manifest).unwrap();tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(tracker.active.load(Ordering::SeqCst),1);assert_eq!(tracker.cancelled.load(Ordering::SeqCst),0);
    stop_tx.send(true).unwrap();finish_ingestion(&mut task,&tracker).await;
}

#[test]
fn health_catalog_tracks_new_pools_and_empty_discovery_is_not_ready() {
    let (_directory,manifest,_reader,_tracker)=fixture(true);
    let mut empty=manifest.clone();empty.pools.clear();
    let mut diagnostics=Diagnostics::new(&empty,false);
    let frame=||HealthFrame { head_observed_at:Some(std::time::Instant::now()), head:None,pools:Vec::new(),journal:None,native_balance:None,native_balance_reason:None,
        queue_len:0,counters:Counters::default(),stage:"running",reason:None };
    let before=diagnostics.snapshot(frame());assert!(before.pools.is_empty());assert_ne!(before.status,"ready");
    diagnostics.update_catalog(&manifest);let after=diagnostics.snapshot(frame());assert_eq!(after.pools.len(),2);assert_ne!(after.status,"ready");
}
