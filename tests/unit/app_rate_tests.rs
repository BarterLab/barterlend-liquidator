//! Local regression for bounded preparation at the public RPC request rate.
use super::*;
use crate::routes::RouteKind;

struct IdentityProbe(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl TxRpc for IdentityProbe {
    async fn identity(&self) -> Result<ChainIdentity> {
        self.0.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
        Err(eyre!("active identity probe"))
    }
    async fn code_hash(&self,_:Address,_:&BlockRef)->Result<alloy::primitives::B256>{unreachable!()}
    async fn head(&self,_:Commitment)->Result<BlockRef>{unreachable!()}
    async fn block(&self,_:u64)->Result<Option<BlockRef>>{unreachable!()}
    async fn receipt(&self,_:alloy::primitives::B256)->Result<Option<Receipt>>{unreachable!()}
    async fn nonce(&self,_:Address,_:bool)->Result<u64>{unreachable!()}
    async fn balance(&self,_:Address)->Result<U256>{unreachable!()}
    async fn gas_price(&self)->Result<U256>{unreachable!()}
    async fn simulate(&self,_:Address,_:&UnsignedTx,_:&BlockRef)->Result<alloy::primitives::Bytes>{unreachable!()}
    async fn estimate(&self,_:Address,_:&UnsignedTx,_:&BlockRef)->Result<u64>{unreachable!()}
    async fn send_raw(&self,_:&alloy::primitives::Bytes)->Result<alloy::primitives::B256>{unreachable!()}
}
fn idle_worker() -> (TxWorker<IdentityProbe>,Arc<IdentityProbe>) {
    use alloy::primitives::B256;
    let identity=JournalIdentity {chain:ChainIdentity {chain_id:31337,genesis_hash:B256::repeat_byte(1)},
        signer:Address::repeat_byte(2),executor:Address::repeat_byte(3),executor_code_hash:B256::repeat_byte(4)};
    let store=JournalStore::new(identity).unwrap();
    let rpc=Arc::new(IdentityProbe(std::sync::atomic::AtomicUsize::new(0)));
    (TxWorker::new(store,rpc.clone(),2,U256::from(100_000),U256::from(30_000)).unwrap(),rpc)
}

#[tokio::test]
async fn idle_cycle_avoids_identity_rpc_but_active_nonce_keeps_full_reconciliation() {
    let (mut worker,rpc)=idle_worker();
    assert_eq!(reconcile_if_active(&mut worker).await.unwrap(),ReconcileOutcome::Idle);
    assert_eq!(rpc.0.load(std::sync::atomic::Ordering::SeqCst),0);
    let signer=worker.store.journal().identity.signer;
    worker.store.update(|journal| {
        journal.active=Some(Intent {reservation:UnsignedTx {chain_id:31337,nonce:0,to:signer,input:alloy::primitives::Bytes::new(),
            gas_limit:21_000,gas_price:U256::from(1)},kind:AttemptKind::Cancel,phase:Phase::ReservedUnsent,attempts:Vec::new(),reserved_at:100});
        Ok(())
    }).unwrap();
    assert!(reconcile_if_active(&mut worker).await.unwrap_err().to_string().contains("active identity probe"));
    assert_eq!(rpc.0.load(std::sync::atomic::Ordering::SeqCst),1);
    assert!(!worker.is_idle());
}

alloy::sol! {
    interface RateToken { function mint(uint256 amount) external; function approve(address spender,uint256 amount) external returns(bool); function balanceOf(address account) external view returns(uint256); }
    interface RateOracle { function setPrice(uint256 newPrice) external; }
}
async fn rate_rpc(client:&reqwest::Client,url:&str,method:&str,params:serde_json::Value)->serde_json::Value {
    let response:serde_json::Value=client.post(url).json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await.unwrap().json().await.unwrap();
    assert!(response.get("error").is_none(),"local setup error: {response}");response["result"].clone()
}
async fn rate_send<C:alloy::sol_types::SolCall>(client:&reqwest::Client,url:&str,from:Address,to:Address,call:C) {
    rate_rpc(client,url,"anvil_impersonateAccount",serde_json::json!([from])).await;
    let hash=rate_rpc(client,url,"eth_sendTransaction",serde_json::json!([{"from":from,"to":to,"data":alloy::primitives::Bytes::from(call.abi_encode()),"gas":"0x989680","gasPrice":"0x77359400"}])).await;
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let receipt=rate_rpc(client,url,"eth_getTransactionReceipt",serde_json::json!([hash])).await;
            if !receipt.is_null() { assert_eq!(receipt["status"],"0x1","setup reverted");break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.unwrap();
}
async fn rate_prepare(manifest:&LocalManifest,snapshot:&StateSnapshot,key:&PositionKey,rate:u64,background:bool)->Result<crate::simulation::SimulatedPlan> {
    let reader=Arc::new(HttpReader::new(&manifest.rpc_url,RpcConfig{min_request_interval:Duration::from_millis(rate),max_retries:0,..Default::default()})?);
    struct BackgroundRead(Option<tokio::task::JoinHandle<()>>);
    impl Drop for BackgroundRead { fn drop(&mut self) { if let Some(task)=&self.0 { task.abort(); } } }
    let _background=BackgroundRead(if background {
        let reader=reader.clone();let pool=key.pool;let at=snapshot.as_of;
        Some(tokio::spawn(async move { loop { let _=reader.read_globals(pool,&at).await;tokio::task::yield_now().await; } }))
    } else { None });
    let rpc=LocalTxRpc::new(&manifest.rpc_url,reader.clone())?;
    let routes=Arc::new(SwapRouteProvider::new(reader.clone(),manifest.v3.clone())?);
    let native_cost=ConfiguredNativeCostOracle::new(routes.clone(),manifest.executor,&[])?;
    let discovered=plan_position(snapshot,key,&manifest.planner)?.remove(0);
    let native_path=routes.configured_routes(discovered.asset.address,manifest.v3.wrapped_native)?.into_iter()
        .find(|path|matches!(path,RouteKind::V3 { fees,.. } if fees==&vec![3000])).expect("seeded native conversion pool");
    native_cost.select_route(discovered.asset.address,&native_path).await?;
    let started=std::time::Instant::now();
    let registry=ProviderRegistry::new(vec![routes.clone()])?;
    let funding=refresh_funding(manifest,&reader,snapshot,key,0).await?;
    assert!(funding.as_of.number>=snapshot.as_of.number,"final preparation must not regress");
    if rate>=67 { assert!(funding.as_of.number>snapshot.as_of.number,"slow discovery must be followed by a new anchor"); }
    let nonce=rpc.nonce(manifest.operator,false).await?;
    assert_eq!(nonce,rpc.nonce(manifest.operator,true).await?);
    let result=crate::selection::compare(&registry,&rpc,&native_cost,&funding,manifest.operator,manifest.executor,nonce,
        manifest.chain.chain_id,&manifest.fees,&SimulationPolicy{max_iterations:6,additional_profit_asset:manifest.additional_profit_asset},
        0,manifest.v3.limits.max_quote_calls.min(manifest.v3.limits.max_candidates),Duration::from_secs(25)).await;
    eprintln!("source comparison at {rate}ms: {:?}",started.elapsed());
    result.map(|selected|selected.plan)
}
#[tokio::test]
#[ignore = "requires isolated seeded Anvil with --block-time 0.3"]
async fn rate_example_full_preparation_survives_public_rate() {
    let url=std::env::var("BARTERLEND_LOCAL_RPC_URL").unwrap();crate::tx::rpc::validate_loopback_url(&url).unwrap();
    let mut manifest:LocalManifest=serde_json::from_str(include_str!("../../examples/local-manifest.json")).unwrap();manifest.rpc_url=url.clone();
    let seed:serde_json::Value=serde_json::from_str(include_str!("../../tests/fixtures/solidity/generated/manifest.json")).unwrap();
    let borrower:Address=serde_json::from_value(seed["borrower_a"].clone()).unwrap();
    let lender:Address=serde_json::from_value(seed["provider_a"].clone()).unwrap();
    let market:u64=seed["market0"].as_str().unwrap().parse().unwrap();
    let pool=manifest.pools.values().next().unwrap();let wad=1_000_000_000_000_000_000u128;
    let client=reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    rate_send(&client,&url,borrower,pool.collateral.address,RateToken::mintCall{amount:U256::from(400*wad)}).await;
    rate_send(&client,&url,borrower,pool.collateral.address,RateToken::approveCall{spender:pool.spec.address,amount:U256::from(400*wad)}).await;
    rate_send(&client,&url,borrower,pool.spec.address,abi::CreditPool::depositCollateralCall{account:borrower,market,amount:400*wad}).await;
    rate_send(&client,&url,borrower,pool.spec.address,abi::CreditPool::placeBorrowOrderCall{account:borrower,market,APR:U256::from(1000),amount:100*wad}).await;
    rate_send(&client,&url,lender,pool.asset.address,RateToken::mintCall{amount:U256::from(100*wad)}).await;
    rate_send(&client,&url,lender,pool.asset.address,RateToken::approveCall{spender:pool.spec.address,amount:U256::from(100*wad)}).await;
    rate_send(&client,&url,lender,pool.spec.address,abi::CreditPool::depositCall{recipient:lender,amount:100*wad}).await;
    rate_send(&client,&url,lender,pool.spec.address,abi::CreditPool::provideExactInCall{account:lender,market,assetsAmount:100*wad,minLoanAmount:0,maxCrossedOrders:10}).await;
    rate_send(&client,&url,borrower,pool.oracle,RateOracle::setPriceCall{newPrice:U256::from(wad/4)}).await;
    let fast=Arc::new(HttpReader::new(&url,RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let mut engine=StateEngine::new(fast,pool.spec,EngineConfig{commitment:Commitment::Latest,expected_identity:Some(manifest.chain),..Default::default()}).unwrap();
    let snapshot=engine.bootstrap().await.unwrap();let key=PositionKey{pool:pool.spec.address,market,borrower};
    let first=std::time::Instant::now();let fast_plan=rate_prepare(&manifest,&snapshot,&key,1,false).await;
    eprintln!("fast preparation {:?}: {}",first.elapsed(),if fast_plan.is_ok(){"OK"}else{"ERROR"});assert!(fast_plan.is_ok(),"baseline fast preparation must succeed: {fast_plan:?}");
    let started=std::time::Instant::now();let slow=rate_prepare(&manifest,&snapshot,&key,67,false).await;
    eprintln!("public-rate preparation {:?}: {:?}",started.elapsed(),slow.as_ref().map(|_|"OK"));
    assert!(slow.is_ok(),"same profitable position cannot finish preparation at public request rate: {slow:?}");
    let plan=slow.unwrap();
    let loaded=rate_prepare(&manifest,&snapshot,&key,67,true).await;
    assert!(loaded.is_ok(),"preparation with competing shared RPC reads failed: {loaded:?}");
    // A previously eligible snapshot must not authorize an execution after recovery.
    rate_send(&client,&url,borrower,pool.oracle,RateOracle::setPriceCall { newPrice:U256::from(wad) }).await;
    let recovery_reader=HttpReader::new(&url,RpcConfig { min_request_interval:Duration::from_millis(1),..Default::default() }).unwrap();
    assert!(refresh_funding(&manifest,&recovery_reader,&snapshot,&key,0).await.is_err(),"stale eligible snapshot must not authorize a healthy current position");
    assert!(plan.metadata().min_asset_profit>U256::ZERO,"zero extra profit must still cover native gas");
}

/// Genuine full runtime, contracts and finalized outcomes under the common public
/// budget. Setup and final assertions use the origin directly, outside the bot.
#[tokio::test]
#[ignore = "requires exclusive seeded Anvil with --block-time 0.3; tests/runners/test-local-rpc-e2e.sh"]
async fn full_bot_public_rpc_budget_finalizes_two_markets_and_restarts_without_duplicate_send() {
    use crate::tx::OutcomeKind;
    let url=std::env::var("BARTERLEND_LOCAL_RPC_URL").unwrap();crate::tx::rpc::validate_loopback_url(&url).unwrap();
    let client=reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    assert_eq!(rate_rpc(&client,&url,"eth_chainId",serde_json::json!([])).await,"0x7a69");
    let seed:serde_json::Value=serde_json::from_str(include_str!("../../tests/fixtures/solidity/generated/manifest.json")).unwrap();
    let mut manifest:LocalManifest=serde_json::from_str(include_str!("../../examples/local-manifest.json")).unwrap();
    manifest.rpc_url=url.clone();
    manifest.rpc=crate::local_config::RpcPolicy::default();manifest.poll_ms=1000;
    manifest.v3.fee_tiers=vec![3000];manifest.v3.intermediates.clear();
    manifest.planner.max_candidates=1;manifest.planner.partial_divisors.clear();
    let multicall:Address="0x0000000000000000000000000000000000000900".parse().unwrap();
    let code:alloy::primitives::Bytes=include_str!("../../tests/fixtures/multicall3-runtime.hex").trim().parse().unwrap();
    rate_rpc(&client,&url,"anvil_setCode",serde_json::json!([multicall,code])).await;
    manifest.rpc.multicall3=Some(Multicall3Config {address:multicall,code_hash:keccak256(&code),..Multicall3Config::canonical()});
    manifest.rpc.read_optimizations.batch_log_hashes=true;
    manifest.sync.current_state=Some(CurrentStateConfig::default());
    manifest.sync.reconcile_user_page_size=128;
    let pool=manifest.pools.values().next().unwrap().clone();
    let lender:Address=serde_json::from_value(seed["provider_a"].clone()).unwrap();
    let borrowers=[serde_json::from_value::<Address>(seed["borrower_a"].clone()).unwrap(),serde_json::from_value::<Address>(seed["borrower_b"].clone()).unwrap()];
    let markets=[seed["market0"].as_str().unwrap().parse::<u64>().unwrap(),seed["market1"].as_str().unwrap().parse::<u64>().unwrap()];
    let wad=1_000_000_000_000_000_000u128;
    rate_send(&client,&url,borrowers[0],pool.oracle,RateOracle::setPriceCall{newPrice:U256::from(wad)}).await;
    for (borrower,market) in borrowers.into_iter().zip(markets) {
        rate_send(&client,&url,borrower,pool.collateral.address,RateToken::mintCall{amount:U256::from(400*wad)}).await;
        rate_send(&client,&url,borrower,pool.collateral.address,RateToken::approveCall{spender:pool.spec.address,amount:U256::from(400*wad)}).await;
        rate_send(&client,&url,borrower,pool.spec.address,abi::CreditPool::depositCollateralCall{account:borrower,market,amount:400*wad}).await;
        rate_send(&client,&url,borrower,pool.spec.address,abi::CreditPool::placeBorrowOrderCall{account:borrower,market,APR:U256::from(1000),amount:100*wad}).await;
        rate_send(&client,&url,lender,pool.asset.address,RateToken::mintCall{amount:U256::from(100*wad)}).await;
        rate_send(&client,&url,lender,pool.asset.address,RateToken::approveCall{spender:pool.spec.address,amount:U256::from(100*wad)}).await;
        rate_send(&client,&url,lender,pool.spec.address,abi::CreditPool::depositCall{recipient:lender,amount:100*wad}).await;
        rate_send(&client,&url,lender,pool.spec.address,abi::CreditPool::provideExactInCall{account:lender,market,assetsAmount:100*wad,minLoanAmount:0,maxCrossedOrders:10}).await;
    }
    rate_send(&client,&url,borrowers[0],pool.oracle,RateOracle::setPriceCall{newPrice:U256::from(wad/4)}).await;
    let inspector=HttpReader::new(&url,RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap();
    let before=inspector.head(Commitment::Latest).await.unwrap();
    let asset_before=inspector.contract_call(pool.asset.address,&RateToken::balanceOfCall{account:manifest.executor},&before).await.unwrap();
    let native_before:U256=serde_json::from_value(rate_rpc(&client,&url,"eth_getBalance",serde_json::json!([manifest.operator,"latest"])).await).unwrap();
    let nonce_before=crate::tx::rpc::quantity_u64(&rate_rpc(&client,&url,"eth_getTransactionCount",serde_json::json!([manifest.operator,"latest"])).await).unwrap();
    assert_eq!(nonce_before,0,"this scenario requires a fresh operator on an exclusive node");
    // Public development key used only by the already validated loopback profile.
    let signer:PrivateKeySigner="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap();
    assert_eq!(signer.address(),manifest.operator);
    let started=std::time::Instant::now();
    let report=tokio::time::timeout(Duration::from_secs(300),run(manifest.clone(),RunOptions{live:true,cycles:40},Some(signer.clone())))
        .await.expect("full bot exceeded its local E2E time budget").unwrap();
    assert_eq!(report.finalized_liquidations,2,"both markets must be liquidated under the shared 15 rps budget");
    assert_eq!(report.observed_active_pending(),Some(false));
    let after=inspector.head(Commitment::Latest).await.unwrap();
    assert!(after.number>before.number+100,"interval mining must progress during full execution");
    for (borrower,market) in borrowers.into_iter().zip(markets) {
        assert_eq!(inspector.contract_call(pool.spec.address,&abi::CreditPool::debtOfCall{market,account:borrower},&after).await.unwrap(),0);
    }
    let asset_after=inspector.contract_call(pool.asset.address,&RateToken::balanceOfCall{account:manifest.executor},&after).await.unwrap();
    let gain=asset_after-asset_before;assert!(gain>U256::ZERO);
    let native_after:U256=serde_json::from_value(rate_rpc(&client,&url,"eth_getBalance",serde_json::json!([manifest.operator,"latest"])).await).unwrap();
    let total_native=report.native_spent;
    assert_eq!(total_native,native_before-native_after);
    assert_eq!(report.last_outcome.as_ref().unwrap().kind,OutcomeKind::Liquidated);
    let nonce_after=crate::tx::rpc::quantity_u64(&rate_rpc(&client,&url,"eth_getTransactionCount",serde_json::json!([manifest.operator,"latest"])).await).unwrap();
    assert_eq!(nonce_after,2,"exactly two signed transactions must be mined, without paid failures");
    let live_elapsed=started.elapsed();let restart_started=std::time::Instant::now();
    let restart=tokio::time::timeout(Duration::from_secs(90),run(manifest.clone(),RunOptions{live:true,cycles:4},Some(signer)))
        .await.expect("restart exceeded bounded local test duration").unwrap();
    assert_eq!(restart.finalized_liquidations,0);assert_eq!(restart.observed_active_pending(),Some(false));
    let final_nonce=crate::tx::rpc::quantity_u64(&rate_rpc(&client,&url,"eth_getTransactionCount",serde_json::json!([manifest.operator,"latest"])).await).unwrap();
    assert_eq!(final_nonce,nonce_after,"restart must not execute completed liquidations again");
    assert_eq!(restart.native_spent,U256::ZERO);
    let result=serde_json::json!({"scenario":"full-bot-two-markets-restart","status":"PASS","rpc_requests_per_second":15,"block_time_ms":300,
        "pools":1,"markets":2,"borrowers":2,"finalized_liquidations":2,"paid_failures":0,"preparation_skips":report.skips,
        "operator_nonce_before":nonce_before,"operator_nonce_after":final_nonce,
        "asset_gain_raw":gain.to_string(),"native_spent_raw":total_native.to_string(),"live_elapsed_ms":live_elapsed.as_millis(),"restart_elapsed_ms":restart_started.elapsed().as_millis(),
        "blocks_during_live":after.number-before.number,"scope":"actual local Anvil, genuine protocol/executor/V3; bounded RAM session accounting"});
    eprintln!("RPC_E2E_RESULT {result}");
    if let Ok(path)=std::env::var("BARTERLEND_RPC_E2E_REPORT") {std::fs::write(path,serde_json::to_vec_pretty(&result).unwrap()).unwrap();}
}
