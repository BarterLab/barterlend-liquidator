//! Real EVM multi-pool and multiple-debts-per-market acceptance, without persistence.
#![allow(dead_code,unused_imports)]
include!("support/app_local_fixture.rs");
use barterlend_liquidator::app::{run,RunOptions};
use alloy::signers::local::PrivateKeySigner;
#[derive(Deserialize)]
struct MultiSeed {
    #[serde(flatten)]
    base: Seed,
    pool_b: Address, oracle_b: Address, pool_b_code_hash: B256, oracle_b_code_hash: B256,
}


#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="fresh two-pool seeded Anvil; tests/runners/test-local-multipool.sh"]
async fn actual_two_pools_multiple_debts_finalize_without_files() {
    let origin=validate_loopback_url(&std::env::var("BARTERLEND_LOCAL_RPC_URL").unwrap()).unwrap();
    let seed:MultiSeed=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").unwrap()).unwrap()).unwrap();
    let mut local=LocalTx{client:reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap(),url:origin.clone(),timestamp:2};
    assert_eq!(local.rpc("eth_blockNumber",json!([])).await,"0x0");
    let reader=Arc::new(HttpReader::new(origin.as_str(),RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.genesis_hash,seed.base.genesis_hash);
    let market=seed.base.market0.parse().unwrap();
    for pool in [seed.base.pool,seed.pool_b] {for borrower in [seed.base.borrower_a,seed.base.borrower_b] {
        open_fault_position(&mut local,&seed.base,pool,borrower,market,400*WAD).await;
    }}
    for oracle in [seed.base.oracle,seed.oracle_b] {
        local.send(seed.base.borrower_a,oracle,TestOracle::setPriceCall{newPrice:U256::from(WAD/2)}).await;
    }
    let at=reader.head(Commitment::Latest).await.unwrap();
    let directory=tempfile::tempdir().unwrap();
    let mut config=manifest(&seed.base,origin.to_string(),chain,keccak256(reader.get_code(seed.base.oracle,&at).await.unwrap()),directory.path());
    let mut second=config.pools["local-pool"].clone();second.spec.address=seed.pool_b;second.code_hash=seed.pool_b_code_hash;
    second.oracle=seed.oracle_b;second.oracle_code_hash=seed.oracle_b_code_hash;config.pools.insert("second-pool".into(),second);
    let mining=LocalTx{client:local.client.clone(),url:origin,timestamp:0};let operator=seed.base.operator;
    let miner=tokio::spawn(async move {
        let deadline=Instant::now()+Duration::from_secs(60);let mut last=0;
        while last<4 {
            let nonce=quantity_u64(&mining.rpc("eth_getTransactionCount",json!([operator,"latest"])).await).unwrap();
            if nonce>last {last=nonce;mining.rpc("anvil_mine",json!(["0x40","0x0"])).await;}
            assert!(Instant::now()<deadline,"not all four debts were liquidated");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let signer:PrivateKeySigner=LOCAL_DEV_KEY.parse().unwrap();
    let report=run(config,RunOptions{live:true,cycles:120},Some(signer)).await.unwrap();miner.await.unwrap();
    assert_eq!(report.finalized_liquidations,4);assert!(!report.active_pending);
    let after=reader.head(Commitment::Latest).await.unwrap();
    for pool in [seed.base.pool,seed.pool_b] {for borrower in [seed.base.borrower_a,seed.base.borrower_b] {
        assert_eq!(reader.contract_call(pool,&CreditPool::debtOfCall{market,account:borrower},&after).await.unwrap(),0);
    }}
    assert_eq!(report.health.unwrap().pools.len(),2);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(),0);
}
