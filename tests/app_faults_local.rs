//! Real EVM transport faults, competition and reorg, observed without storage files.
#![allow(dead_code,unused_imports)]
include!("support/app_local_fixture.rs");
use barterlend_liquidator::{app::{run_observed,RunOptions},diagnostics::HealthSnapshot};
use alloy::signers::local::PrivateKeySigner;

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="fresh exclusive seeded local Anvil; tests/runners/test-local-bot.sh faults"]
async fn actual_rpc_send_failure_and_reorg_recover_without_files() {
    let origin=validate_loopback_url(&std::env::var("BARTERLEND_LOCAL_RPC_URL").unwrap()).unwrap();
    let seed:Seed=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").unwrap()).unwrap()).unwrap();
    let mut local=LocalTx{client:reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap(),url:origin.clone(),timestamp:2};
    assert_eq!(local.rpc("eth_chainId",json!([])).await,"0x7a69");
    assert_eq!(local.rpc("eth_blockNumber",json!([])).await,"0x0");
    let reader=Arc::new(HttpReader::new(origin.as_str(),RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.genesis_hash,seed.genesis_hash);
    let market=seed.market0.parse().unwrap();
    open_fault_position(&mut local,&seed,seed.pool,seed.borrower_a,market,400*WAD).await;
    local.send(seed.borrower_a,seed.oracle,TestOracle::setPriceCall{newPrice:U256::from(WAD/2)}).await;
    let at=reader.head(Commitment::Latest).await.unwrap();let oracle_hash=keccak256(reader.get_code(seed.oracle,&at).await.unwrap());
    let proxy=FaultProxy::new(origin);proxy.stats.lock().unwrap().block_raw=true;
    let directory=tempfile::tempdir().unwrap();let mut config=manifest(&seed,proxy.url.clone(),chain,oracle_hash,directory.path());
    config.v3.fee_tiers=vec![3000];config.recovery.retry_initial_ms=100;config.recovery.retry_max_ms=200;
    let observation=Arc::new(std::sync::RwLock::new(None::<HealthSnapshot>));
    let signer:PrivateKeySigner=LOCAL_DEV_KEY.parse().unwrap();
    let snapshot=local.rpc("evm_snapshot",json!([])).await;
    let mut app=AppTask(Some(tokio::spawn(run_observed(config,RunOptions{live:true,cycles:200},Some(signer),observation.clone()))));
    wait_for("actual send blocked",Duration::from_secs(30),|| (proxy.stats.lock().unwrap().blocked_raw>0).then_some(())).await;
    let original=proxy.raw_requests()[0].clone();let original_hash=keccak256(&original);
    // New debt is observed while the sender retains the uncertain nonce in RAM.
    open_fault_position(&mut local,&seed,seed.pool,seed.borrower_b,market,800*WAD).await;
    let changed=reader.head(Commitment::Latest).await.unwrap();
    wait_for("ingestion continues during uncertain send",Duration::from_secs(10),||{
        let h=observation.read().unwrap();let h=h.as_ref()?;
        h.pools.iter().any(|p|p.debt_positive==Some(2)&&p.as_of.is_some_and(|b|b.number>=changed.number)).then_some(())
    }).await;
    proxy.stats.lock().unwrap().outage=true;
    wait_for("outage is visible",Duration::from_secs(10),||{
        let h=observation.read().unwrap();h.as_ref().filter(|h|h.status!="ready").map(|_|())
    }).await;
    {let mut p=proxy.stats.lock().unwrap();p.outage=false;p.block_raw=false;}
    wait_for("mined but not finalized",Duration::from_secs(20),||{
        observation.read().unwrap().as_ref().filter(|h|h.tx.phase=="mined").map(|_|())
    }).await;
    let receipt=local.rpc("eth_getTransactionReceipt",json!([original_hash])).await;assert_eq!(receipt["status"],"0x1");
    // Revert the canonical chain to before signing; actual old receipt disappears.
    assert_eq!(local.rpc("evm_revert",json!([snapshot])).await,true);
    local.rpc("evm_setNextBlockTimestamp",json!([at.timestamp+2])).await;
    local.rpc("evm_mine",json!([])).await;
    let deadline=Instant::now()+Duration::from_secs(20);
    loop {
        let now=local.rpc("eth_getTransactionReceipt",json!([original_hash])).await;
        if now["status"]=="0x1" && now["blockHash"]!=receipt["blockHash"] {break;}
        assert!(Instant::now()<deadline,"reorged transaction was not safely rebroadcast");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
    let report=app.finish().await;
    assert_eq!(report.finalized_liquidations,1);assert!(!report.active_pending);
    let outcome=report.last_outcome.unwrap();assert_eq!(outcome.kind,OutcomeKind::Liquidated);assert_eq!(outcome.attempt.hash,original_hash);
    assert!(proxy.raw_requests().iter().all(|raw|raw==&original),"retry/reorg must reuse identical signed bytes");
    assert_eq!(local.rpc("eth_getTransactionCount",json!([seed.operator,"latest"])).await,"0x1");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(),0,"runtime must not write files");
}
