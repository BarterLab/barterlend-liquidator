use std::{collections::BTreeMap, future::{Future, poll_fn}, pin::Pin, sync::Arc, task::Poll, time::{Duration,Instant}};
use alloy::{primitives::{Address, B256, U256, keccak256}, signers::local::PrivateKeySigner, sol_types::SolCall};
use barterlend_state::*;
#[cfg(test)]
use barterlend_state::rpc::RpcConfig;
use color_eyre::eyre::{Result, eyre};
use tokio::sync::watch;
use futures_util::FutureExt;
use crate::{local_config::LocalManifest, executor_abi::LiquidationExecutor, planner::plan_position,
    diagnostics::{Diagnostics, HealthFrame, PoolObservation, Counters, emit_error, emit_stdout, safe_reason, unix_ms},
    routes::{SwapRouteProvider, ProviderRegistry, LiquidityProvider}, economics::{ConfiguredNativeCostOracle,NativeCostOracle},
    simulation::SimulationPolicy, scheduler::{Scheduler,Candidate},
    tx::{*, rpc::{LocalTxRpc, TxRpc}}};
use crate::recovery::{EndpointHealth,RecoveryBackoff};

#[derive(Clone, Debug)]
pub struct RunOptions { pub live: bool, pub cycles: u64 }

#[derive(Clone)]
struct PoolView { snapshot: Option<Arc<StateSnapshot>>, verified: bool, last_success_unix_ms: Option<u64>, last_error: Option<&'static str> }

fn snapshot_anchor_is_current(at: &BlockRef, head: &BlockRef, max_age_blocks: u64) -> bool {
    at.timestamp <= head.timestamp && at.number <= head.number
        && (at.number != head.number || at.hash == head.hash)
        && head.number - at.number <= max_age_blocks
}

#[derive(Clone, Debug, Default)]
pub struct RunReport {
    pub dry_run_plans: u64, pub finalized_liquidations: u64, pub skips: u64,
    /// Authoritative only when journal_observed; default false does not prove an idle live journal.
    pub active_pending: bool,
    /// The live journal is loaded and usable; false also covers an uncertain commit.
    pub journal_observed: bool,
    pub native_spent: U256,
    pub pending_hash: Option<B256>,
    /// Bounded final evidence for embedded callers/tests. Never logged as raw signed bytes.
    pub last_outcome: Option<Outcome>,
    /// Last bounded diagnostic snapshot, useful for embedding and acceptance tests.
    pub health: Option<crate::diagnostics::HealthSnapshot>,
}

impl RunReport {
    /// None means no authoritative live ledger observation, including an uncertain commit.
    pub fn observed_active_pending(&self) -> Option<bool> {
        self.journal_observed.then_some(self.active_pending)
    }
}

/// A poisoned cache cannot attest the financial state changed by an uncertain commit.
pub fn usable_journal(store:Option<&JournalStore>) -> Option<&Journal> {
    store.map(JournalStore::journal)
}

pub async fn preflight(manifest: &LocalManifest, reader: &HttpReader) -> Result<()> {
    manifest.validate()?;
    if reader.identity().await? != manifest.chain { return Err(eyre!("chain/genesis does not match manifest")); }
    let at = reader.head(Commitment::Latest).await?;
    if keccak256(reader.get_code(manifest.executor, &at).await?) != manifest.executor_code_hash { return Err(eyre!("executor bytecode mismatch")); }
    let owner = reader.contract_call(manifest.executor, &LiquidationExecutor::ownerCall {}, &at).await?;
    let operator = reader.contract_call(manifest.executor, &LiquidationExecutor::operatorsCall { operator: manifest.operator }, &at).await?;
    let router = reader.contract_call(manifest.executor, &LiquidationExecutor::routerCall {}, &at).await?;
    let factory = reader.contract_call(manifest.executor, &LiquidationExecutor::factoryCall {}, &at).await?;
    if owner != manifest.owner || !operator || router != manifest.v3.router || factory != manifest.v3.factory { return Err(eyre!("executor roles/router mismatch")); }
    crate::catalog::verify_binding(reader, manifest, &at).await?;
    verify_v4_binding(manifest, reader, &at).await?;
    for pool in manifest.pools.values() {
        let descriptor = reader.describe_pool(&pool.spec, &at).await?;
        if descriptor.code_hash != pool.code_hash || descriptor.asset != pool.asset || descriptor.collateral != pool.collateral || descriptor.oracle != pool.oracle
            || keccak256(reader.get_code(pool.oracle, &at).await?) != pool.oracle_code_hash
        { return Err(eyre!("pool/token/oracle identity mismatch")); }
        let approval = reader.contract_call(manifest.executor, &LiquidationExecutor::poolConfigCall { pool: pool.spec.address }, &at).await?;
        if !crate::catalog::tokens_verified(reader,manifest.executor,pool.asset.address,pool.collateral.address,&at).await? { return Err(eyre!("pool tokens not verified")); }
        if approval.asset != pool.asset.address || approval.collateral != pool.collateral.address || !approval.recognized { return Err(eyre!("pool not recognized by executor factory")); }
    }
    reader.ensure_canonical(&at).await?;
    Ok(())
}

async fn verify_v4_binding(manifest: &LocalManifest, reader: &HttpReader, at: &BlockRef) -> Result<()> {
    if let Some(kyber) = &manifest.kyber {
        if reader.contract_call(manifest.executor, &LiquidationExecutor::kyberRouterCall {}, at).await? != kyber.router
            || reader.contract_call(manifest.executor, &LiquidationExecutor::kyberRouterCodeHashCall {}, at).await? != kyber.router_code_hash {
            return Err(eyre!("executor Kyber binding differs from configuration"));
        }
    }

    if let Some(v4) = &manifest.v4 {
        if reader.contract_call(manifest.executor, &LiquidationExecutor::v4ManagerCall {}, at).await? != v4.manager {
            return Err(eyre!("executor V4 manager differs from manifest"));
        }
    }
    Ok(())
}

// Recovery of a saved nonce needs only trusted chain/executor identity. New plans
// additionally validate current roles and the selected pool, independently of others.
fn decode_read<C: SolCall>(bytes: &alloy::primitives::Bytes) -> Result<C::Return> {
    Ok(crate::abi_validation::decode_return::<C>(bytes)?)
}

async fn preflight_for_position(manifest:&LocalManifest,reader:&HttpReader,key:&PositionKey,at:&BlockRef) -> Result<PoolDescriptor> {
    if reader.identity().await?!=manifest.chain { return Err(eyre!("position RPC identity mismatch")); }
    let pool=manifest.pools.values().find(|pool|pool.spec.address==key.pool).ok_or_else(||eyre!("candidate pool missing from manifest"))?;
    let reads=reader.read_views(&[
        PinnedRead::Code { address:manifest.executor },
        PinnedRead::call(manifest.executor,&LiquidationExecutor::ownerCall {}),
        PinnedRead::call(manifest.executor,&LiquidationExecutor::operatorsCall { operator:manifest.operator }),
        PinnedRead::call(manifest.executor,&LiquidationExecutor::routerCall {}),
        PinnedRead::call(manifest.executor,&LiquidationExecutor::factoryCall {}),
        PinnedRead::Code { address:pool.oracle },
        PinnedRead::call(manifest.executor,&LiquidationExecutor::poolConfigCall { pool:key.pool }),
        PinnedRead::call(manifest.executor,&LiquidationExecutor::verifiedTokensCall { token:pool.asset.address }),
        PinnedRead::call(manifest.executor,&LiquidationExecutor::verifiedTokensCall { token:pool.collateral.address }),
    ],at).await?;
    if keccak256(&reads[0])!=manifest.executor_code_hash { return Err(eyre!("executor bytecode mismatch")); }
    let owner=decode_read::<LiquidationExecutor::ownerCall>(&reads[1])?;
    let operator=decode_read::<LiquidationExecutor::operatorsCall>(&reads[2])?;
    let router=decode_read::<LiquidationExecutor::routerCall>(&reads[3])?;
    let factory=decode_read::<LiquidationExecutor::factoryCall>(&reads[4])?;
    if owner!=manifest.owner || !operator || router!=manifest.v3.router || factory!=manifest.v3.factory { return Err(eyre!("executor roles/router/factory changed")); }
    verify_v4_binding(manifest, reader, at).await?;
    let descriptor=reader.describe_pool(&pool.spec,at).await?;
    if descriptor.code_hash!=pool.code_hash || descriptor.asset!=pool.asset || descriptor.collateral!=pool.collateral || descriptor.oracle!=pool.oracle
        || keccak256(&reads[5])!=pool.oracle_code_hash { return Err(eyre!("candidate pool/token/oracle identity mismatch")); }
    let approval=decode_read::<LiquidationExecutor::poolConfigCall>(&reads[6])?;
    if !approval.recognized || approval.asset!=pool.asset.address || approval.collateral!=pool.collateral.address { return Err(eyre!("candidate pool no longer recognized")); }
    if !decode_read::<LiquidationExecutor::verifiedTokensCall>(&reads[7])?
        || !decode_read::<LiquidationExecutor::verifiedTokensCall>(&reads[8])? { return Err(eyre!("candidate tokens no longer verified")); }
    reader.ensure_canonical(at).await?;
    Ok(descriptor)
}

async fn ingest_pools<R: StateReader + 'static>(manifest: LocalManifest, reader: Arc<R>, _live: bool, tx: watch::Sender<BTreeMap<Address,PoolView>>, stop: watch::Receiver<bool>) -> Result<()> {
    let mut tasks=tokio::task::JoinSet::new();
    for pool in manifest.pools.values().cloned() {
        let reader=reader.clone();let tx=tx.clone();let mut stop=stop.clone();let manifest=manifest.clone();
        tasks.spawn(async move {
            let config=EngineConfig { commitment:Commitment::Latest,expected_identity:Some(manifest.chain),log_chunk_size:manifest.sdk_log_chunk_size,
                history_chunks_per_step:manifest.sdk_history_chunks_per_step, event_driven_refresh:true,
                read_batch_size:manifest.rpc.read_optimizations.position_batch_size,
                reconciliation_interval:Duration::from_secs(manifest.sync.reconciliation_interval_seconds),
                reconcile_positions_per_sync:manifest.sync.reconcile_positions_per_round,
                reconcile_user_pages_per_sync:manifest.sync.reconcile_user_pages_per_round,
                reconcile_user_page_size:manifest.sync.reconcile_user_page_size,..Default::default() };
            let mut engine:Option<crate::pool_sync::PoolSync<R>>=None;
            let mut view=PoolView { snapshot:None,verified:false,last_success_unix_ms:None,last_error:None };
            let mut backoff=RecoveryBackoff::new(manifest.recovery.clone());
            loop {
                if *stop.borrow() { break; }
                if engine.is_none() {
                    let restored=crate::pool_sync::PoolSync::new(reader.clone(),pool.spec,config.clone(),&manifest.sync);
                    match restored {
                        Ok(restored)=>{ view.snapshot=restored.snapshot_shared();engine=Some(restored); },
                        Err(error)=>{ emit_error("state_restore_recovery_waiting",&error,None);view.last_error=Some("state_restore_unavailable"); },
                    }
                }
                let mut delay=backoff.next_delay();
                if let Some(engine)=engine.as_mut() {
                    let result=tokio::select! {
                        biased;
                        changed=stop.changed()=>{ if changed.is_err() || *stop.borrow() { break; } continue; },
                        result=engine.sync_step()=>result,
                    };
                    #[cfg(test)]
                    if std::env::var_os("BARTERLEND_LOAD_REPORT").is_some() {
                        if let Err(error)=&result { eprintln!("LOAD_SYNC_ERROR {error:?}"); }
                    }
                    view.snapshot=engine.snapshot_shared();view.verified=false;
                    match result {
                        Ok(SyncStepOutcome::Complete(_))=>{
                            if view.last_success_unix_ms.is_none() {
                                emit_stdout(&serde_json::json!({"event":"pool_synced","pool":pool.spec.address,
                                    "block":view.snapshot.as_ref().map(|s|s.as_of.number),
                                    "positions":view.snapshot.as_ref().map(|s|s.positions.len())}));
                            }
                            view.verified=true;
                            if view.verified { view.last_success_unix_ms=Some(unix_ms()); }
                            view.last_error=None;
                            backoff.reset();delay=Duration::from_millis(manifest.poll_ms);
                        },
                        Ok(SyncStepOutcome::Progress(progress))=>{
                            view.last_error=Some("state_discovery_in_progress");backoff.reset();delay=Duration::from_millis(manifest.poll_ms);
                            emit_stdout(&serde_json::json!({"event":"state_discovery_progress","pool":pool.spec.address,"through_block":progress.through.map(|b|b.number),"target_block":progress.target.number,"discovered_positions":progress.discovered_positions}));
                        },
                        Err(error)=>{
                            view.last_error=Some("state_sync_failed");
                            crate::diagnostics::emit_stderr(&serde_json::json!({"event":"diagnostic_error","code":"state_sync_failed",
                                "pool":pool.spec.address,"failure":crate::diagnostics::rpc_failure(&error.into())}));
                        },
                    }
                }
                tx.send_modify(|views| { views.insert(pool.spec.address,view.clone()); });
                if tx.is_closed() { break; }
                tokio::select! {
                    _=tokio::time::sleep(delay)=>{},
                    changed=stop.changed()=>{ if changed.is_err() || *stop.borrow() { break; } },
                }
            }
            Ok::<(),color_eyre::eyre::Report>(())
        });
    }
    drop(tx);
    while let Some(result)=tasks.join_next().await { result??; }
    Ok(())
}

// Catalog updates start only new workers. Existing snapshots, transaction
// state and synchronization cursors remain alive when another pool is added.
async fn ingest_catalog<R:StateReader+'static>(mut catalog:watch::Receiver<LocalManifest>, reader:Arc<R>, live:bool,
    tx:watch::Sender<BTreeMap<Address,PoolView>>, mut stop:watch::Receiver<bool>) -> Result<()> {
    let mut started=std::collections::BTreeSet::new();let mut workers=tokio::task::JoinSet::new();
    loop {
        if *stop.borrow() || tx.is_closed() { break; }
        let manifest=catalog.borrow_and_update().clone();
        for pool in manifest.pools.values() {
            if !started.insert(pool.spec.address) { continue; }
            let mut one=manifest.clone();one.pools.retain(|_,p|p.spec.address==pool.spec.address);
            let reader=reader.clone();let tx=tx.clone();let mut stop=stop.clone();let address=pool.spec.address;
            workers.spawn(async move {
                let mut backoff=RecoveryBackoff::new(one.recovery.clone());
                loop {
                    if *stop.borrow() || tx.is_closed() { break; }
                    let result=std::panic::AssertUnwindSafe(ingest_pools(one.clone(),reader.clone(),live,tx.clone(),stop.clone())).catch_unwind().await;
                    if *stop.borrow() || tx.is_closed() { break; }
                    if let Ok(Err(error))=&result { emit_error("state_ingestion_restarting",error,None); }
                    tx.send_modify(|views| { if let Some(view)=views.get_mut(&address) { view.verified=false;view.last_error=Some("state_ingestion_restarting"); } });
                    tokio::select! { _=stop.changed()=>break,_=tokio::time::sleep(backoff.next_delay())=>{} }
                }
            });
        }
        tokio::select! {
            _=stop.changed()=>break,
            changed=catalog.changed()=>{ if changed.is_err() { break; } },
            result=workers.join_next(),if !workers.is_empty()=>{
                if !*stop.borrow() { return Err(eyre!("pool supervisor stopped: {:?}",result)); }
            },
        }
    }
    workers.abort_all();while workers.join_next().await.is_some() {} Ok(())
}

fn liquidity(manifest:&LocalManifest,reader:&Arc<HttpReader>) -> Result<(Arc<ProviderRegistry>,ConfiguredNativeCostOracle)> {
    let uniswap=Arc::new(SwapRouteProvider::new(reader.clone(),manifest.v3.clone())?.with_v4(manifest.v4.clone())?);
    let native=ConfiguredNativeCostOracle::new(uniswap.clone(),manifest.executor,&manifest.native_price_feeds)?;
    let mut providers:Vec<Arc<dyn LiquidityProvider>>=vec![uniswap];
    if let Some(config)=&manifest.kyber { providers.push(Arc::new(crate::routes::kyber::KyberProvider::new(reader.clone(),manifest.chain,config.clone(),&manifest.v3.limits)?)); }
    Ok((Arc::new(ProviderRegistry::new(providers)?),native))
}

fn aggregate_spend(journal: &Journal) -> Result<U256> {
    journal.native_spent()
}

fn observe_terminal(journal:&Journal,observed:&mut u64,pending:&mut Option<Candidate>,scheduler:&mut Scheduler,report:&mut RunReport,now:u64) -> Result<()> {
    let count=journal.outcome_count()?;
    if count<*observed { return Err(eyre!("session terminal history regressed")); }
    if count==*observed { return Ok(()); }
    let outcome=journal.outcomes.last().ok_or_else(||eyre!("new terminal outcome missing"))?;
    emit_stdout(&serde_json::json!({"event":"finalized","kind":format!("{:?}",outcome.kind),"native_spend":aggregate_spend(journal)?,"outcome_count":count,
        "outcome":crate::diagnostics::public_outcome(outcome)}));
    if count-*observed==1 && outcome.kind==OutcomeKind::Liquidated { report.finalized_liquidations+=1; }
    if let Some(candidate)=pending.take() {
        if outcome.kind==OutcomeKind::Liquidated { scheduler.succeeded(&candidate.key); }
        else { scheduler.failed(&candidate,now)?; }
    }
    *observed=count;
    Ok(())
}

pub fn max_pending_native_bound(journal: &Journal) -> Result<U256> {
    let Some(active) = &journal.active else { return Ok(U256::ZERO); };
    if active.attempts.is_empty() { return active.reservation.native_bound(); }
    active.attempts.iter().try_fold(U256::ZERO, |maximum, attempt| Ok(maximum.max(attempt.envelope.native_bound()?)))
}

async fn publish_health(diagnostics: &mut Diagnostics, manifest: &LocalManifest, rpc: &LocalTxRpc,
    views: &BTreeMap<Address, PoolView>, head: Option<(BlockRef,Instant)>, journal: Option<&Journal>,
    queue_len: usize, report: &RunReport, stage: &'static str, reason: Option<&'static str>, force: bool,
) {
    if !diagnostics.due(force) { return; }
    // An unhealthy provider has already failed the control probe. Telemetry
    // must not delay recovery with another doomed balance request.
    if head.is_none() {
        publish_health_frame(diagnostics,manifest,views,None,journal,queue_len,report,stage,reason,force,None,Some("native_balance_unavailable"));
        return;
    }
    let balance = rpc.balance(manifest.operator).await;
    let (native_balance, native_balance_reason) = match balance {
        Ok(balance) => (Some(balance), None),
        Err(_) => (None, Some("native_balance_unavailable")),
    };
    publish_health_frame(diagnostics,manifest,views,head,journal,queue_len,report,stage,reason,force,native_balance,native_balance_reason);
}

fn publish_health_frame(diagnostics: &mut Diagnostics, manifest: &LocalManifest,
    views: &BTreeMap<Address, PoolView>, head: Option<(BlockRef,Instant)>, journal: Option<&Journal>,
    queue_len: usize, report: &RunReport, stage: &'static str, reason: Option<&'static str>, force: bool,
    native_balance: Option<U256>, native_balance_reason: Option<&'static str>,
) {
    let pools = manifest.pools.values().map(|configured| {
        let view = views.get(&configured.spec.address);
        PoolObservation { pool: configured.spec.address, snapshot: view.and_then(|view| view.snapshot.as_deref()),
            verified: view.is_some_and(|view| view.verified), last_success_unix_ms: view.and_then(|view| view.last_success_unix_ms),
            last_error: view.and_then(|view| view.last_error) }
    }).collect();
    let frame = HealthFrame { head:head.map(|v|v.0), head_observed_at:head.map(|v|v.1), pools, journal, native_balance, native_balance_reason, queue_len,
        counters: Counters { dry_run_plans: report.dry_run_plans, finalized_liquidations: report.finalized_liquidations, skips: report.skips },
        stage, reason };
    if let Err(error) = diagnostics.publish(frame, force) { emit_error("health_write_failed", &error, None); }
}

pub fn ensure_operator_gate(journal: &Journal) -> Result<()> {
    if journal.outcomes.last().is_some_and(|outcome| outcome.kind == OutcomeKind::InvalidSuccessEvent) { return Err(eyre!("successful receipt has invalid executor event; operator review required")); }
    Ok(())
}

pub async fn startup_reconcile<R: TxRpc>(worker: &mut TxWorker<R>) -> Result<ReconcileOutcome> {
    ensure_operator_gate(worker.store.journal())?;
    let outcome = worker.reconcile().await?;
    // Reconciliation itself may discover a new invalid terminal receipt.
    ensure_operator_gate(worker.store.journal())?;
    if outcome == ReconcileOutcome::UnknownNonceConsumed { return Err(eyre!("unknown signer nonce consumption; journal retained")); }
    Ok(outcome)
}

pub fn ensure_send_budget(journal: &Journal, max_total: U256, max_price: U256, max_gas_limit: u64, cleanup_reserve: U256) -> Result<()> {
    let attempt = journal.active.as_ref().and_then(|active| active.attempts.last()).ok_or_else(|| eyre!("no signed attempt"))?;
    let reserve = if matches!(attempt.kind,AttemptKind::Cancel) { U256::ZERO } else { cleanup_reserve };
    let possible = aggregate_spend(journal)?.checked_add(max_pending_native_bound(journal)?).and_then(|v| v.checked_add(reserve)).ok_or_else(|| eyre!("send budget overflow"))?;
    if possible > max_total || attempt.envelope.gas_price > max_price || attempt.envelope.gas_limit > max_gas_limit { return Err(eyre!("saved send exceeds current total native/fee/gas policy; journal retained")); }
    Ok(())
}

async fn broadcast_budgeted<R: TxRpc>(worker: &mut TxWorker<R>, manifest: &LocalManifest, native_cost: &dyn NativeCostOracle) -> Result<()> {
    ensure_send_budget(worker.store.journal(),manifest.max_total_native_spend,manifest.fees.max_gas_price,manifest.fees.max_gas_limit,manifest.fees.native_reserve)?;
    let attempt = worker.store.journal().active.as_ref().and_then(|active|active.attempts.last()).ok_or_else(|| eyre!("signed attempt missing"))?;
    if let AttemptKind::Liquidation(meta) = &attempt.kind {
        let at = worker.rpc().head(Commitment::Latest).await?;
        if at.timestamp > meta.deadline || at.number > meta.max_valid_block { return Err(eyre!("signed quote expired before send")); }
        let cost = native_cost.quote_cost(meta.asset,attempt.envelope.native_bound()?,&at).await?;
        cost.validate(meta.asset,attempt.envelope.native_bound()?,&at)?;
        let required = cost.cost_asset.checked_add(manifest.additional_profit_asset).ok_or_else(|| eyre!("current cost guard overflow"))?;
        if meta.min_asset_profit < required { return Err(eyre!("saved profit guard no longer covers current native cost; require fresh replacement")); }
        let result = worker.broadcast_with_cost(&cost,manifest.additional_profit_asset).await;
        ensure_operator_gate(worker.store.journal())?;
        return result;
    }
    let result = worker.broadcast().await;
    ensure_operator_gate(worker.store.journal())?;
    result
}

async fn shutdown_requested(mut shutdown: Pin<&mut impl Future<Output=std::io::Result<()>>>) -> std::io::Result<bool> {
    poll_fn(|cx| match shutdown.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(Ok(false)),
        Poll::Ready(result) => Poll::Ready(result.map(|()| true)),
    }).await
}

// Only unsigned, read-only work belongs here. Reservation, signing, sending and
// reconciliation must finish in-memory journal transitions without cancellation.
async fn unsigned_until_shutdown<T>(work: impl Future<Output=T>, mut shutdown: Pin<&mut impl Future<Output=std::io::Result<()>>>) -> std::io::Result<Option<T>> {
    tokio::select! {
        biased;
        signal = shutdown.as_mut() => { signal?; Ok(None) },
        result = work => Ok(Some(result)),
    }
}

// The SDK engine chunk and the HTTP reader range must share the same limit.
// Otherwise a larger configured chunk is recursively split before HTTP I/O.
pub fn state_reader(manifest: &LocalManifest) -> Result<HttpReader> {
    manifest.rpc.validate()?;
    let mut transport=HttpReader::with_endpoints(&rpc_endpoints(manifest),manifest.rpc.transport_config(manifest.sdk_log_chunk_size))?
        .with_completion_rate_limit(manifest.rpc.requests_per_second as usize)?;
    if let Some(config)=&manifest.rpc.multicall3 { transport=transport.with_multicall3(config.clone())?; }
    transport=transport.with_read_optimizations(manifest.rpc.read_optimizations.clone())?;
    Ok(transport.control_reader(manifest.rpc.bulk_config(manifest.sdk_log_chunk_size))?)
}

pub fn rpc_endpoints(manifest: &LocalManifest) -> Vec<String> {
    std::iter::once(manifest.rpc_url.clone()).chain(manifest.rpc_fallback_urls.iter().cloned()).collect()
}

async fn healthy_head(manifest: &LocalManifest, reader: &HttpReader, monitors: &EndpointHealth) -> Result<BlockRef> {
    let initial=reader.rpc().endpoint_index();
    for offset in 0..reader.rpc().endpoint_count() {
        let index=(initial+offset)%reader.rpc().endpoint_count();
        reader.rpc().select_endpoint(index)?;
        let started=Instant::now();
        let observation=tokio::time::timeout(Duration::from_secs(5),async {
            if reader.identity().await?!=manifest.chain { return Err(eyre!("recovery endpoint identity mismatch")); }
            let head=reader.head(Commitment::Latest).await?;
            monitors.observe(index,head,unix_ms()/1000)?;
            Ok::<BlockRef,color_eyre::eyre::Report>(head)
        }).await;
        match observation {
            Ok(Ok(head)) => {
                if index!=initial { emit_stdout(&serde_json::json!({"event":"rpc_endpoint_recovered","endpoint_index":index,"block_number":head.number})); }
                return Ok(head);
            },
            Ok(Err(error)) => emit_stdout(&serde_json::json!({"event":"rpc_check_failed","endpoint_index":index,
                "operation":"identity_and_head","elapsed_ms":started.elapsed().as_millis() as u64,
                "failure":crate::diagnostics::rpc_failure(&error)})),
            Err(_) => emit_stdout(&serde_json::json!({"event":"rpc_check_failed","endpoint_index":index,
                "operation":"identity_and_head","elapsed_ms":started.elapsed().as_millis() as u64,
                "failure":{"reason":"health_check_timeout","timeout_ms":5000}})),
        }
    }
    Err(eyre!("all configured RPC endpoints unavailable or stale"))
}

pub async fn run(manifest: LocalManifest, options: RunOptions, signer: Option<PrivateKeySigner>) -> Result<RunReport> {
    run_observed(manifest,options,signer,Arc::new(std::sync::RwLock::new(None))).await
}

/// Embed the runtime with a bounded read-only observation handle. No financial state is exposed.
pub async fn run_observed(manifest: LocalManifest, options: RunOptions, signer: Option<PrivateKeySigner>,
    health:Arc<std::sync::RwLock<Option<crate::diagnostics::HealthSnapshot>>>) -> Result<RunReport> {
    run_until(manifest,options,signer,health,crate::server::shutdown_signal()).await
}

/// Embed with an explicit shutdown signal; the same signing barrier and grace apply.
pub async fn run_until(manifest: LocalManifest, options: RunOptions, signer: Option<PrivateKeySigner>,
    health:Arc<std::sync::RwLock<Option<crate::diagnostics::HealthSnapshot>>>,
    shutdown:impl Future<Output=std::io::Result<()>>) -> Result<RunReport> {
    let (stop, mut stopped) = watch::channel(false);
    let signing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    tokio::pin!(shutdown);
    let work = run_inner(manifest, options, signer, signing.clone(), health, async move {
        stopped.wait_for(|v| *v).await.map_err(|_| std::io::Error::other("shutdown channel closed"))?; Ok(())
    });
    tokio::pin!(work);
    tokio::select! {
        result = work.as_mut() => result,
        signal = shutdown.as_mut() => {
            signal?;
            signing.store(false, std::sync::atomic::Ordering::Release);
            let _ = stop.send(true);
            tokio::time::timeout(Duration::from_secs(20), work.as_mut()).await
                .map_err(|_| eyre!("shutdown grace expired; last broadcast may remain pending"))?
        }
    }
}

struct AbortTask(tokio::task::AbortHandle);
impl Drop for AbortTask { fn drop(&mut self) { self.0.abort(); } }

async fn run_inner(manifest: LocalManifest, options: RunOptions, signer: Option<PrivateKeySigner>,
    signing: Arc<std::sync::atomic::AtomicBool>, health:Arc<std::sync::RwLock<Option<crate::diagnostics::HealthSnapshot>>>, shutdown: impl Future<Output=std::io::Result<()>>,
) -> Result<RunReport> {
    manifest.validate()?;
    if options.live && signer.as_ref().is_none_or(|s| s.address() != manifest.operator) { return Err(eyre!("live requires the configured operator key")); }
    if !options.live && signer.is_some() { return Err(eyre!("dry-run must remain keyless")); }
    tokio::pin!(shutdown);
    // Register before the first RPC and retain this receiver throughout the run.
    // Recreating ctrl_c in each select loses signals received during RPC work.
    if shutdown_requested(shutdown.as_mut()).await? { return Ok(RunReport::default()); }
    emit_stdout(&serde_json::json!({"event":"bot_starting","mode":if options.live {"live"} else {"dry_run"},
        "chain_id":manifest.chain.chain_id,"executor":manifest.executor,"operator":manifest.operator,
        "configured_pools":manifest.pools.len(),"factory_discovery":manifest.discovery.is_some(),
        "uniswap_v3":true,"uniswap_v4":manifest.v4.is_some(),"kyber":manifest.kyber.is_some()}));
    let mut diagnostics = Diagnostics::new(&manifest, options.live).with_health(health);
    let progress=Arc::new(crate::server::Progress::default());
    let _health_server = crate::server::start(manifest.health_listen, diagnostics.health(), progress.clone()).await?;
    let bulk_reader=Arc::new(state_reader(&manifest)?);
    let reader=Arc::new(bulk_reader.control_reader(manifest.rpc.control_config(manifest.sdk_log_chunk_size))?);
    let (routes,native_cost)=liquidity(&manifest,&reader)?;
    let monitors=Arc::new(EndpointHealth::new(manifest.recovery.clone(),manifest.profile!="local",reader.rpc().endpoint_count()));
    let mut startup_backoff=RecoveryBackoff::new(manifest.recovery.clone());
    let mut startup_attempts=0u64;
    loop {
        progress.tick();
        let initialized = unsigned_until_shutdown(tokio::time::timeout(Duration::from_secs(60),async {
            healthy_head(&manifest,&reader,&monitors).await?;
            Ok::<(),color_eyre::eyre::Report>(())
        }),shutdown.as_mut()).await?;
        let Some(initialized)=initialized else { return Ok(RunReport::default()); };
        if matches!(initialized,Ok(Ok(()))) { break; }
        emit_stdout(&serde_json::json!({"event":"startup_recovery_waiting","reason":"preflight_unavailable","endpoint_index":reader.rpc().endpoint_index()}));
        startup_attempts+=1;
        if options.cycles!=0 && startup_attempts>=options.cycles { return Err(eyre!("bounded startup preflight remains unavailable")); }
        let index=(reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count();reader.rpc().select_endpoint(index)?;
        if unsigned_until_shutdown(tokio::time::sleep(startup_backoff.next_delay()),shutdown.as_mut()).await?.is_none() { return Ok(RunReport::default()); }
    }
    let mut rpc=LocalTxRpc::for_endpoints(&rpc_endpoints(&manifest),reader.clone(),&manifest.profile,manifest.chain)?;
    rpc.set_health_monitor(monitors.clone());
    let rpc=Arc::new(rpc);
    let mut worker = if options.live {
        let identity = JournalIdentity { chain:manifest.chain,signer:manifest.operator,executor:manifest.executor,executor_code_hash:manifest.executor_code_hash };
        let store=JournalStore::new(identity)?;
        let mut worker = TxWorker::new(store,rpc.clone(),manifest.max_attempts_per_nonce,manifest.fees.max_fee_native,manifest.fees.native_reserve)?;
        worker.set_signing_gate(signing.clone());
        worker.set_max_gas_price(manifest.fees.max_gas_price)?;
        worker.set_cancellation_retry_seconds(manifest.cancellation_retry_seconds)?;
        // The recoverable main loop reconciles before any send or reservation.
        Some(worker)
    } else { None };
    let (view_tx, mut view_rx) = watch::channel(BTreeMap::new());
    let (stop_tx,stop_rx) = watch::channel(false);
    let (catalog_tx,catalog_rx)=watch::channel(manifest.clone());
    let discovery=if manifest.discovery.is_some() {
        Some(tokio::spawn(crate::catalog::watch_catalog(bulk_reader.clone(),catalog_tx.clone(),stop_rx.clone())))
    } else { None };
    let _discovery_guard=discovery.as_ref().map(|task|AbortTask(task.abort_handle()));
    let mut ingestion=tokio::spawn(ingest_catalog(catalog_rx.clone(),bulk_reader.clone(),options.live,view_tx,stop_rx));
    let _ingestion_guard = AbortTask(ingestion.abort_handle());
    let mut report = RunReport { journal_observed: worker.is_some(), ..Default::default() };
    // Recovery of an already signed transaction does not depend on a new census.
    // Keep the large execution future off callers' async stacks, including
    // repeated embedded runs and the default Tokio worker stack.
    let result = Box::pin(run_loop(&manifest,&options,signer.as_ref(),&reader,&routes,&native_cost,&rpc,&mut worker,&mut view_rx,&mut diagnostics,&mut report,&monitors,&progress,Some(catalog_rx),shutdown.as_mut())).await;
    let _ = stop_tx.send(true);
    match tokio::time::timeout(Duration::from_secs(20),&mut ingestion).await {
        Ok(Ok(Ok(()))) => {},
        Ok(Ok(Err(error))) if result.is_ok() => return Err(error),
        Ok(Err(error)) if result.is_ok() => return Err(error.into()),
        Err(_) => { ingestion.abort(); let _ = ingestion.await; }
        _ => {},
    }
    if let Err(error) = &result {
        let views = view_rx.borrow().clone();
        let sampled=Instant::now();
        publish_health(&mut diagnostics,&manifest,&rpc,&views,reader.head(Commitment::Latest).await.ok().map(|head|(head,sampled)),
            usable_journal(worker.as_ref().map(|worker|&worker.store)),0,&report,"blocked",Some(safe_reason(error)),true).await;
    }
    let _=diagnostics.flush(Duration::from_millis(500)).await;
    result
}

async fn reconcile_if_active<R:TxRpc>(worker:&mut TxWorker<R>) -> Result<ReconcileOutcome> {
    if worker.is_idle() {
        // The loop has checked the endpoint with healthy_head. Keep the ledger's
        // uncertainty/DB ownership check even when there is no nonce to reconcile.
        // Reservation, signing and sending retain their own fresh identity checks.
        Ok(ReconcileOutcome::Idle)
    } else {
        worker.reconcile().await
    }
}

async fn run_loop(manifest: &LocalManifest, options: &RunOptions, signer: Option<&PrivateKeySigner>, reader: &Arc<HttpReader>, routes: &Arc<ProviderRegistry>, native_cost: &ConfiguredNativeCostOracle, rpc: &Arc<LocalTxRpc>, worker: &mut Option<TxWorker<LocalTxRpc>>, view_rx: &mut watch::Receiver<BTreeMap<Address,PoolView>>, diagnostics: &mut Diagnostics, report: &mut RunReport, monitors: &EndpointHealth, progress: &crate::server::Progress, mut catalog:Option<watch::Receiver<LocalManifest>>, mut shutdown: Pin<&mut impl Future<Output=std::io::Result<()>>>) -> Result<RunReport> {
    let mut scheduler = Scheduler::new(manifest.queue_capacity,3,manifest.cooldown_seconds)?;
    let mut cycles = 0u64;
    let retry_interval = Duration::from_millis(manifest.poll_ms).max(Duration::from_secs(1));
    let mut next_broadcast_retry = std::time::Instant::now();
    let mut pending_candidate: Option<Candidate> = None;
    let mut last_observed_head = None;
    let mut recovery_backoff=RecoveryBackoff::new(manifest.recovery.clone());
    let first_observation_deadline=std::time::Instant::now()+Duration::from_secs(60);
    let mut observed_outcomes=worker.as_ref().map(|w|w.store.journal().outcome_count()).transpose()?.unwrap_or(0);
    let mut refreshed:Option<(LocalManifest,Arc<ProviderRegistry>,ConfiguredNativeCostOracle)>=None;
    loop {
        progress.tick();
        if let Some(catalog)=catalog.as_mut() {
            // Keep the active transaction's selected native valuation route alive.
            // New pool workers may synchronize while execution finishes.
            if worker.as_ref().is_none_or(TxWorker::is_idle) && catalog.has_changed().unwrap_or(false) {
                let updated=catalog.borrow_and_update().clone();
                let (routes,native)=liquidity(&updated,reader)?;
                diagnostics.update_catalog(&updated);
                refreshed=Some((updated,routes,native));
            }
        }
        let (manifest,routes,native_cost)=refreshed.as_ref().map(|(m,r,n)|(m,r,n)).unwrap_or((manifest,routes,native_cost));
        if shutdown_requested(shutdown.as_mut()).await? { break; }
        if view_rx.has_changed().is_err() { return Err(eyre!("state ingestion stopped")); }
        let awaiting_initial_state = !view_rx.borrow().values().any(|view|view.verified) && worker.as_ref().is_none_or(TxWorker::is_idle);
        if options.cycles!=0 && awaiting_initial_state && std::time::Instant::now()>=first_observation_deadline {
            emit_stdout(&serde_json::json!({"event":"bounded_state_startup_incomplete","reason":"state_unavailable"}));break;
        }
        let recovery_observed_at=Instant::now();
        let healthy=unsigned_until_shutdown(healthy_head(manifest,reader,monitors),shutdown.as_mut()).await?;
        let Some(healthy)=healthy else { break; };
        let recovery_head=match healthy {
            Ok(head)=>head,
            Err(error)=>{
            emit_error("rpc_recovery_waiting",&error,None);
            let views=view_rx.borrow().clone();
            if unsigned_until_shutdown(publish_health(diagnostics,manifest,rpc,&views,None,usable_journal(worker.as_ref().map(|worker|&worker.store)),scheduler.len(),report,"rpc_paused",Some("rpc_unhealthy"),false),shutdown.as_mut()).await?.is_none() { break; }
            cycles+=1;if options.cycles!=0 && cycles>=options.cycles { break; }
            if unsigned_until_shutdown(tokio::time::sleep(recovery_backoff.next_delay()),shutdown.as_mut()).await?.is_none() { break; }
                continue;
            },
        };
        let mut recovering=false;
        if let Some(worker) = worker {
            observe_terminal(worker.store.journal(),&mut observed_outcomes,&mut pending_candidate,&mut scheduler,report,recovery_head.timestamp)?;
            if let Err(error)=ensure_operator_gate(worker.store.journal()) {
                emit_error("operator_review_required",&error,None);
                let views=view_rx.borrow().clone();
                if unsigned_until_shutdown(publish_health(diagnostics,manifest,rpc,&views,None,Some(worker.store.journal()),scheduler.len(),report,"operator_required",Some("invalid_success_event"),false),shutdown.as_mut()).await?.is_none() { break; }
                cycles+=1;if options.cycles!=0 && cycles>=options.cycles { break; }
                if unsigned_until_shutdown(tokio::time::sleep(Duration::from_millis(manifest.recovery.retry_max_ms)),shutdown.as_mut()).await?.is_none() { break; }
                continue;
            }
            let reconciled = reconcile_if_active(worker).await;
            if shutdown_requested(shutdown.as_mut()).await? { break; }
            match reconciled {
                Ok(ReconcileOutcome::Finalized(_)) => {},
                Ok(ReconcileOutcome::ReservedNeedsRefresh) => {
                    if let Err(error)=worker.release_unsent().await { recovering=true;emit_error("ledger_recovery_waiting",&error,None); }
                },
                Ok(ReconcileOutcome::MinedUnfinalized) => {
                    // A healthy Latest does not prove that the provider's Finalized
                    // index progresses. Ask the next trusted provider next cycle.
                    recovering=true;
                    emit_stdout(&serde_json::json!({"event":"finality_recovery_waiting","reason":"finalized_index_not_ready"}));
                    reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                },
                Ok(ReconcileOutcome::Expired | ReconcileOutcome::CancelReplacementDue) => {
                    let signer = signer.ok_or_else(|| eyre!("live signer missing"))?;
                    let cancellation=async {
                        let price=worker.next_cancel_price(rpc.gas_price().await?)?;
                        let cost=U256::from(21_000).checked_mul(price).ok_or_else(||eyre!("cancel fee overflow"))?;
                        let possible=max_pending_native_bound(worker.store.journal())?.max(cost);
                        if aggregate_spend(worker.store.journal())?.checked_add(possible).ok_or_else(||eyre!("cancel budget overflow"))?>manifest.max_total_native_spend { return Err(eyre!("cancellation total budget exhausted")); }
                        worker.cancel(price,signer).await?;
                        broadcast_budgeted(worker,manifest,native_cost).await
                    }.await;
                    if let Err(error)=cancellation {
                        recovering=true;emit_error("cancellation_recovery_waiting",&error,None);
                        // A price/attempt cap prevents a new signature, not recovery
                        // of an existing cancellation whose previous send was uncertain.
                        if worker.store.journal().active.as_ref()
                            .and_then(|active|active.attempts.last()).is_some_and(|attempt|matches!(attempt.kind,AttemptKind::Cancel)) {
                            if let Err(error)=broadcast_budgeted(worker,manifest,native_cost).await {
                                emit_error("saved_cancellation_broadcast_paused",&error,None);
                            }
                        }
                        reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                    }
                }
                Ok(ReconcileOutcome::Pending) if std::time::Instant::now() >= next_broadcast_retry => {
                    next_broadcast_retry = std::time::Instant::now() + retry_interval;
                    if let Err(error) = broadcast_budgeted(worker,manifest,native_cost).await {
                        recovering=true;emit_error("saved_broadcast_paused", &error, None);
                        reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                    }
                }
                Ok(ReconcileOutcome::UnknownNonceConsumed) => {
                    emit_stdout(&serde_json::json!({"event":"nonce_indexing_recovery_waiting","reason":"unknown_nonce_consumed"}));
                    let views=view_rx.borrow().clone();
                    publish_health_frame(diagnostics,manifest,&views,Some((recovery_head,recovery_observed_at)),Some(worker.store.journal()),scheduler.len(),report,"recovery_waiting",Some("unknown_nonce_consumed"),false,None,Some("native_balance_unavailable"));
                    reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                    cycles+=1;if options.cycles!=0 && cycles>=options.cycles { break; }
                    if unsigned_until_shutdown(tokio::time::sleep(recovery_backoff.next_delay()),shutdown.as_mut()).await?.is_none() { break; }
                    continue;
                },
                Err(error) => {
                    emit_error("transaction_reconciliation_paused", &error, None);
                    reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                    let views = view_rx.borrow().clone();
                    if unsigned_until_shutdown(publish_health(diagnostics,manifest,rpc,&views,None,usable_journal(Some(&worker.store)),scheduler.len(),report,"rpc_paused",Some("transaction_reconciliation_paused"),false),shutdown.as_mut()).await?.is_none() { break; }
                    cycles += 1;
                    if options.cycles != 0 && cycles >= options.cycles { break; }
                    if unsigned_until_shutdown(tokio::time::sleep(recovery_backoff.next_delay()),shutdown.as_mut()).await?.is_none() { break; }
                    continue;
                }
                _ => {},
            }
            observe_terminal(worker.store.journal(),&mut observed_outcomes,&mut pending_candidate,&mut scheduler,report,recovery_head.timestamp)?;
            if ensure_operator_gate(worker.store.journal()).is_err() { continue; }
        }
        let views = view_rx.borrow().clone();
        let head_observed_at=Instant::now();
        let Some(head_result) = unsigned_until_shutdown(reader.head(Commitment::Latest),shutdown.as_mut()).await? else { break; };
        let head = match head_result {
            Ok(head) => head,
            Err(error) => {
                emit_error("latest_head_unavailable", &error, None);
                if unsigned_until_shutdown(publish_health(diagnostics,manifest,rpc,&views,None,usable_journal(worker.as_ref().map(|worker|&worker.store)),scheduler.len(),report,"rpc_paused",Some("latest_head_unavailable"),false),shutdown.as_mut()).await?.is_none() { break; }
                cycles += 1;
                if options.cycles != 0 && cycles >= options.cycles { break; }
                tokio::select! { biased; signal = shutdown.as_mut() => { signal?; break; }, _ = tokio::time::sleep(Duration::from_millis(manifest.poll_ms)) => {} }
                continue;
            }
        };
        last_observed_head = Some((head,head_observed_at));
        let mut current = Vec::new();
        for (pool, view) in &views {
            match view.snapshot.as_ref().filter(|snapshot| view.verified && snapshot_anchor_is_current(&snapshot.as_of,&head,manifest.v3.limits.max_age_blocks)) {
                Some(snapshot) => current.push(snapshot),
                None => scheduler.invalidate_pool(*pool),
            }
        }
        if current.len() == manifest.pools.len() {
            if let Some(worker) = worker.as_mut() {
                let keys = current.iter().flat_map(|snapshot| snapshot.positions.values().filter(|p| p.amounts.debt > 0).map(|p| p.key)).collect();
                worker.store.retain_failures(&keys);
            }
        }
        scheduler.ingest_shared(&current,head.timestamp)?;
        if scheduler.deferred_len() > 0 && diagnostics.due(false) {
            emit_stdout(&serde_json::json!({"event":"queue_backpressure","queued":scheduler.len(),"deferred":scheduler.deferred_len(),"capacity":manifest.queue_capacity}));
        }
        let idle = worker.as_ref().is_none_or(TxWorker::is_idle);
        if idle && !recovering {
            if let Some(candidate) = scheduler.pop_ready(head.timestamp) {
                let view = views.get(&candidate.key.pool).filter(|v| v.verified).and_then(|v| v.snapshot.as_ref());
                if let Some(snapshot) = view.filter(|s| s.epoch == candidate.epoch && snapshot_anchor_is_current(&s.as_of,&head,manifest.v3.limits.max_age_blocks)) {
                    // Candidate search can contain many individually bounded RPC
                    // calls. Bound the entire unsigned search as well, so one pool
                    // cannot hold up subsequent candidates for their combined limits.
                    let preparation = unsigned_until_shutdown(async {
                        tokio::time::timeout(Duration::from_secs(60),async {
                            let plans=plan_position(snapshot,&candidate.key,&manifest.planner)?;
                            if plans.is_empty() { return Err(eyre!("no liquidatable funding candidates")); }
                            let native_paths=native_cost.paths(snapshot.pool.asset.address)?;
                            if native_paths.is_empty() { return Err(eyre!("no configured liquidation/native route")); }
                            let mut last_error=eyre!("no profitable liquidation combination");
                            // Compare sources for one amount/valuation route at a time.
                            // Rotate failed amounts and native paths without an unbounded grid.
                            for _ in 0..2 {
                                let attempt=scheduler.begin_probe(&candidate);
                                let (funding_index,round,native_index)=comparison_indices(attempt,plans.len(),native_paths.len());
                                let result=async {
                                    native_cost.select_route(snapshot.pool.asset.address,&native_paths[native_index]).await?;
                                    let funding=refresh_funding(manifest,reader,snapshot,&candidate.key,funding_index).await?;
                                    if let Some(worker)=worker.as_mut() {
                                        if worker.store.paid_failure_count(&funding.key,funding.paid_state_fingerprint()?).await?>=manifest.max_paid_failures_per_state {
                                            return Err(eyre!("session paid failure budget exhausted for this borrower state"));
                                        }
                                    }
                                    let nonce=rpc.nonce(manifest.operator,false).await?;
                                    if nonce!=rpc.nonce(manifest.operator,true).await? { return Err(eyre!("operator has untracked pending nonce")); }
                                    let selected=reader.verified_read(Box::pin(crate::selection::compare(routes,rpc.as_ref(),native_cost,&funding,
                                        manifest.operator,manifest.executor,nonce,manifest.chain.chain_id,&manifest.fees,
                                        &SimulationPolicy {max_iterations:6,additional_profit_asset:manifest.additional_profit_asset},round,
                                        manifest.v3.limits.max_quote_calls.min(manifest.v3.limits.max_candidates),Duration::from_secs(25)))).await??;
                                    diagnostics.quoted(&funding.key,&funding.as_of,U256::from(funding.expected.seized_collateral),std::slice::from_ref(&selected.quote));
                                    emit_stdout(&serde_json::json!({"event":"route_selected","key":funding.key,"source":selected.quote.route.source(),
                                        "attempted":selected.attempted,"simulated":selected.simulated,"net_profit_asset":selected.plan.net_profit_asset(),
                                        "gas_limit":selected.plan.envelope().gas_limit}));
                                    Ok(selected.plan)
                                }.await;
                                match result { Ok(plan)=>return Ok(plan),Err(error)=>last_error=error }
                            }
                            Err(last_error)
                        }).await.map_err(|_|eyre!("unsigned candidate preparation time budget exhausted"))?
                    },shutdown.as_mut()).await?;
                    let Some(preparation) = preparation else { break; };
                    match preparation {
                        Ok(plan) => {
                            if shutdown_requested(shutdown.as_mut()).await? { break; }
                            diagnostics.validated(&candidate.key,&plan.metadata().as_of);
                            if let Some(worker) = worker {
                                let spend = aggregate_spend(worker.store.journal())?;
                                let committed = spend.checked_add(plan.envelope().native_bound()?).and_then(|v| v.checked_add(manifest.fees.native_reserve)).ok_or_else(|| eyre!("total budget overflow"))?;
                                if committed > manifest.max_total_native_spend {
                                    recovering=true;report.skips+=1;
                                    emit_stdout(&serde_json::json!({"event":"budget_recovery_waiting","reason":"total_budget_exhausted"}));
                                    scheduler.failed(&candidate,head.timestamp)?;
                                } else {
                                    let execution=async {
                                        worker.reserve(plan).await?;
                                        pending_candidate=Some(candidate.clone());
                                        worker.sign_reserved(signer.ok_or_else(||eyre!("live signer missing"))?).await?;
                                        next_broadcast_retry=std::time::Instant::now()+retry_interval;
                                        broadcast_budgeted(worker,manifest,native_cost).await
                                    }.await;
                                    if let Err(error)=execution {
                                        recovering=true;emit_error("execution_recovery_waiting",&error,Some(&candidate.key));
                                        reader.rpc().select_endpoint((reader.rpc().endpoint_index()+1)%reader.rpc().endpoint_count())?;
                                    }
                                }
                            } else { report.dry_run_plans += 1; emit_stdout(&serde_json::json!({"event":"dry_run_validated","key":candidate.key,"gas_limit":plan.envelope().gas_limit,"min_asset_profit":plan.metadata().min_asset_profit})); scheduler.failed(&candidate,head.timestamp)?; }
                        }
                        Err(error) => { report.skips += 1; diagnostics.skipped(&candidate.key,safe_reason(&error)); emit_stdout(&serde_json::json!({"event":"position_skipped","key":candidate.key,"reason":safe_reason(&error)})); emit_error("position_preparation_failed", &error, Some(&candidate.key)); scheduler.failed(&candidate,head.timestamp)?; }
                    }
                }
            }
        }
        if let Some(worker)=worker { observe_terminal(worker.store.journal(),&mut observed_outcomes,&mut pending_candidate,&mut scheduler,report,head.timestamp)?; }
        let stage=if recovering { "recovery_waiting" } else { "running" };
        if unsigned_until_shutdown(publish_health(diagnostics,manifest,rpc,&views,Some((head,head_observed_at)),usable_journal(worker.as_ref().map(|worker|&worker.store)),scheduler.len(),report,stage,None,false),shutdown.as_mut()).await?.is_none() { break; }
        if !recovering { recovery_backoff.reset(); }
        // A bounded idle run must get a first observation, even if the census is slow.
        // An existing signed nonce always consumes its recovery cycles immediately.
        if !awaiting_initial_state { cycles += 1; }
        if options.cycles != 0 && cycles >= options.cycles { break; }
        let delay=if recovering { recovery_backoff.next_delay() } else { Duration::from_millis(manifest.poll_ms) };
        tokio::select! { biased; signal = shutdown.as_mut() => { signal?; break; }, _ = tokio::time::sleep(delay) => {}, changed = view_rx.changed(), if !recovering => { if changed.is_err() { return Err(eyre!("state ingestion stopped")); } } }
    }
    let journal=usable_journal(worker.as_ref().map(|worker|&worker.store));
    report.journal_observed=journal.is_some();
    report.active_pending=journal.is_some_and(|journal|journal.active.is_some());
    report.native_spent=journal.map(Journal::native_spent).transpose()?.unwrap_or(U256::ZERO);
    report.last_outcome=journal.and_then(|j| j.outcomes.last()).cloned();
    report.pending_hash=journal.and_then(|j| j.active.as_ref()).and_then(|a| a.attempts.last()).map(|a| a.hash);
    let views = view_rx.borrow().clone();
    // Exit telemetry starts no RPC: this is the last observed head, and the
    // absent balance explicitly says it was not refreshed during shutdown.
    let manifest=refreshed.as_ref().map_or(manifest,|(m,_,_)|m);
    publish_health_frame(diagnostics,manifest,&views,last_observed_head,journal,scheduler.len(),report,"stopped",None,true,None,Some("shutdown_not_refreshed"));
    report.health=diagnostics.health().read().ok().and_then(|h|h.clone());
    Ok(report.clone())
}

// Cover all sizes/native valuations before advancing the bounded route sample.
fn comparison_indices(attempt:u64,funding:usize,native:usize)->(usize,u64,usize) {
    let size=attempt%funding as u64;
    let valuation=(attempt/funding as u64+size)%native as u64;
    (size as usize,attempt/(funding*native) as u64,valuation as usize)
}

// All competing quotes and simulations use the same freshly verified funding.
async fn refresh_funding(manifest:&LocalManifest,reader:&HttpReader,snapshot:&StateSnapshot,
    key:&PositionKey,funding_index:usize) -> Result<crate::planner::FundingPlan> {
    reader.verified_read(Box::pin(async {
        let at=reader.head(Commitment::Latest).await?;
        let descriptor=preflight_for_position(manifest,reader,key,&at).await?;
        let fresh=refresh_verified_position(reader,snapshot,key,&at,&descriptor).await?;
        let plans=plan_position(&fresh,key,&manifest.planner)?;
        plans.get(funding_index).cloned().ok_or_else(||eyre!("funding candidate changed"))
    })).await?
}

#[cfg(test)]
#[path="../tests/unit/app_ingestion_tests.rs"]
mod ingestion_tests;

// Descriptor and risk reads share the verified funding anchor.
async fn refresh_verified_position(reader: &HttpReader, snapshot: &StateSnapshot, key: &PositionKey, at: &BlockRef, descriptor: &PoolDescriptor) -> Result<StateSnapshot> {
    use barterlend_state::abi::CreditPool;
    if descriptor != &snapshot.pool { return Err(eyre!("fresh pool identity changed")); }
    let values=reader.read_pool_views(key.pool,&[
        PinnedRead::call(key.pool,&CreditPool::debtOfCall { market:key.market,account:key.borrower }),
        PinnedRead::call(key.pool,&CreditPool::collateralOfCall { market:key.market,account:key.borrower }),
        PinnedRead::call(key.pool,&CreditPool::getLiquidationLTVCall {}),
        PinnedRead::call(key.pool,&CreditPool::getStalePriceWindowCall {}),
        PinnedRead::call(key.pool,&CreditPool::getMinDebtRemainingAfterLiquidationCall {}),
        PinnedRead::call(key.pool,&CreditPool::getLiquidationGracePeriodCall { market:key.market }),
        PinnedRead::call(key.pool,&CreditPool::getLiquidationDiscountMinCall { market:key.market }),
        PinnedRead::call(key.pool,&CreditPool::getLiquidationDiscountMaxCall { market:key.market }),
    ],at).await?;
    let amounts=PositionAmounts { debt:decode_read::<CreditPool::debtOfCall>(&values[0])?,collateral:decode_read::<CreditPool::collateralOfCall>(&values[1])? };
    let globals=PoolRiskConfig { liquidation_ltv:decode_read::<CreditPool::getLiquidationLTVCall>(&values[2])?,
        stale_window:decode_read::<CreditPool::getStalePriceWindowCall>(&values[3])?,min_debt_remaining:decode_read::<CreditPool::getMinDebtRemainingAfterLiquidationCall>(&values[4])? };
    let market=MarketRiskConfig { grace_period:decode_read::<CreditPool::getLiquidationGracePeriodCall>(&values[5])?,
        discount_min:decode_read::<CreditPool::getLiquidationDiscountMinCall>(&values[6])?,discount_max:decode_read::<CreditPool::getLiquidationDiscountMaxCall>(&values[7])? };
    // SDK verifies source/target before price and retains invalid/stale status.
    let oracle=reader.read_oracle(descriptor,at).await?;
    let risk=barterlend_state::risk::evaluate_risk(&amounts,key.market,&globals,&market,&oracle,at.timestamp);
    // Build only the selected position instead of cloning the entire pool census.
    Ok(StateSnapshot { globals,markets:BTreeMap::from([(key.market,market)]),oracle,as_of:*at,
        positions:BTreeMap::from([(*key,PositionRecord { key:*key,amounts,risk,last_changed:*at })]),
        identity:snapshot.identity,spec:snapshot.spec,pool:snapshot.pool.clone(),abi_fingerprint:snapshot.abi_fingerprint,epoch:snapshot.epoch })
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize,Ordering};

    #[tokio::test]
    async fn shutdown_cancels_blocked_unsigned_read_before_execution() {
        struct ActiveRead(Arc<AtomicUsize>);
        impl Drop for ActiveRead { fn drop(&mut self) { self.0.fetch_sub(1,Ordering::SeqCst); } }
        let active = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let executed = Arc::new(AtomicUsize::new(0));
        let (stop_tx,stop_rx) = tokio::sync::oneshot::channel();
        let read = active.clone();let ready = entered.clone();let execution = executed.clone();
        let task = tokio::spawn(async move {
            let shutdown = async { stop_rx.await.map_err(|_| std::io::Error::other("shutdown channel closed")) };
            tokio::pin!(shutdown);
            let result = unsigned_until_shutdown(async {
                read.fetch_add(1,Ordering::SeqCst);
                let guard = ActiveRead(read);
                ready.notify_one();
                std::future::pending::<()>().await;
                drop(guard);
            },shutdown.as_mut()).await.unwrap();
            if result.is_some() { execution.fetch_add(1,Ordering::SeqCst); }
        });
        tokio::time::timeout(Duration::from_secs(2),entered.notified()).await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst),1);
        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2),task).await.unwrap().unwrap();
        assert_eq!(active.load(Ordering::SeqCst),0,"blocked read future was not dropped");
        assert_eq!(executed.load(Ordering::SeqCst),0,"cancelled preparation reached execution");
    }

    #[tokio::test]
    async fn already_received_shutdown_wins_over_ready_unsigned_plan() {
        let shutdown = async { Ok::<(),std::io::Error>(()) };
        tokio::pin!(shutdown);
        assert!(unsigned_until_shutdown(std::future::ready(42),shutdown.as_mut()).await.unwrap().is_none());
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn queued_snapshot_must_still_be_current_before_preparation() {
        let head=BlockRef { number:100,hash:alloy::primitives::B256::repeat_byte(1),parent_hash:alloy::primitives::B256::repeat_byte(2),timestamp:1000,commitment:Commitment::Latest };
        assert!(snapshot_anchor_is_current(&head,&head,10));
        let mut at=head;at.number=90;at.timestamp=990;assert!(snapshot_anchor_is_current(&at,&head,10));
        at.number=89;assert!(!snapshot_anchor_is_current(&at,&head,10));
        at=head;at.hash=alloy::primitives::B256::repeat_byte(3);assert!(!snapshot_anchor_is_current(&at,&head,10));
        at=head;at.number+=1;assert!(!snapshot_anchor_is_current(&at,&head,10));
        at=head;at.number-=1;at.timestamp+=1;assert!(!snapshot_anchor_is_current(&at,&head,10));
    }
}

#[cfg(test)]
mod reader_config_tests {
    use super::*;
    use std::{io::{BufRead,BufReader,Read,Write},net::TcpListener,time::Instant};
    use serde_json::{Value,json};

    #[tokio::test]
    async fn manifest_log_chunk_reaches_http_reader_without_local_subdivision() {
        for chunk in [100u64,5000] {
            let listener=TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url=format!("http://{}",listener.local_addr().unwrap());
            let server=std::thread::spawn(move || {
                let deadline=Instant::now()+Duration::from_secs(3);
                let (mut stream,_)=loop {
                    match listener.accept() {
                        Ok(stream)=>break stream,
                        Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>{
                            assert!(Instant::now()<deadline,"configured log request never reached HTTP");
                            std::thread::sleep(Duration::from_millis(5));
                        },
                        Err(error)=>panic!("local HTTP accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut input=BufReader::new(stream.try_clone().unwrap());
                let mut length=None;
                loop {
                    let mut line=String::new();
                    assert!(input.read_line(&mut line).unwrap()>0);
                    if line=="\r\n" {break;}
                    if let Some((name,value))=line.split_once(':') && name.eq_ignore_ascii_case("content-length") {
                        length=Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let length=length.unwrap();assert!(length<65536);
                let mut body=vec![0;length];input.read_exact(&mut body).unwrap();
                let request:Value=serde_json::from_slice(&body).unwrap();
                let response=json!({"jsonrpc":"2.0","id":request["id"],"result":[]}).to_string();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).unwrap();
                request
            });
            let mut manifest:LocalManifest=serde_json::from_str(include_str!("../examples/local-manifest.json")).unwrap();
            manifest.rpc_url=url;manifest.sdk_log_chunk_size=chunk;
            let reader=state_reader(&manifest).unwrap();
            let defaults=RpcConfig::default();let config=reader.rpc().config();
            assert_eq!(config.max_log_range,chunk);
            assert_eq!(config.request_timeout,defaults.request_timeout);
            assert_eq!(config.max_response_bytes,defaults.max_response_bytes);
            assert_eq!(config.max_logs,defaults.max_logs);
            assert_eq!(config.max_retries,defaults.max_retries);
            assert_eq!(config.pinning,defaults.pinning);
            let pool=manifest.pools.values().next().unwrap().spec.address;
            let from=12345;let to=from+chunk-1;
            let logs=tokio::time::timeout(Duration::from_secs(2),reader.logs(pool,from,to)).await.unwrap().unwrap();
            assert!(logs.is_empty());
            let request=server.join().unwrap();
            assert_eq!(request["method"],"eth_getLogs");
            assert_eq!(request["params"],json!([{"address":pool,"fromBlock":format!("0x{from:x}"),"toBlock":format!("0x{to:x}")} ]));
            assert!(reader.logs(pool,from,to+1).await.is_err(),"configured upper limit remains enforced");
        }
    }
}

#[cfg(test)]
#[path="../tests/unit/app_rate_tests.rs"]
mod rate_tests;

#[cfg(test)]
mod probe_tests {
    use super::*;
    #[test]
    fn bounded_comparison_grid_covers_sizes_and_native_paths_before_next_route_sample() {
        assert_eq!(comparison_indices(0,12,2),(0,0,0));
        assert_eq!(comparison_indices(1,12,2),(1,0,1));
        let seen:std::collections::BTreeSet<_>=(0..24).map(|step|comparison_indices(step,12,2)).collect();
        assert_eq!(seen.len(),24);
        assert!(seen.iter().all(|(_,round,_)|*round==0));
        assert_eq!(comparison_indices(24,12,2),(0,1,0));
        assert_eq!(comparison_indices(2,3,1),(2,0,0));
    }
}

#[cfg(test)]
#[path="../tests/unit/app_load_tests.rs"]
mod load_tests;
