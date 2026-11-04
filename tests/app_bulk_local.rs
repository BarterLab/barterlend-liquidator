//! Real pre-seeded debts and genuine atomic execution; all external traffic is loopback.
#![allow(dead_code,unused_imports)]
include!("support/app_local_fixture.rs");
use barterlend_liquidator::app::{run_until,RunOptions};
use alloy::signers::local::PrivateKeySigner;
use std::sync::RwLock;
sol! {interface BulkToken {function balanceOf(address account) external view returns(uint256);}}
#[derive(Deserialize)]
struct BulkPool {address:Address,oracle:Address,code_hash:B256,oracle_code_hash:B256}
#[derive(Deserialize)]
struct BulkSeed {#[serde(flatten)] base:Seed,market1:String,credit_pools:Vec<BulkPool>,positions_per_market:usize,eligible_per_market:usize}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="isolated 1200/4000 debt Anvil; BARTERLEND_BULK_POOLS=4 or 8 tests/runners/test-local-multipool.sh"]
async fn actual_bulk_census_and_atomic_liquidations_at_15rps() {
    let origin=validate_loopback_url(&std::env::var("BARTERLEND_LOCAL_RPC_URL").unwrap()).unwrap();
    let proxy=validate_loopback_url(&std::env::var("BARTERLEND_BULK_PROXY_URL").unwrap()).unwrap();
    let seed:BulkSeed=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").unwrap()).unwrap()).unwrap();
    let total=seed.credit_pools.len()*2*seed.positions_per_market;
    assert!(matches!(total,1200|4000));let expected=seed.credit_pools.len()*2*seed.eligible_per_market;assert_eq!(expected,32);
    let local=LocalTx{client:reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap(),url:origin.clone(),timestamp:2};
    let reader=Arc::new(HttpReader::new(origin.as_str(),RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.chain_id,31337);assert_eq!(chain.genesis_hash,seed.base.genesis_hash);
    let dir=tempfile::tempdir().unwrap();let mut config=manifest(&seed.base,proxy.to_string(),chain,seed.credit_pools[0].oracle_code_hash,dir.path());
    let template=config.pools.values().next().unwrap().clone();config.pools.clear();
    for (index,pool) in seed.credit_pools.iter().enumerate() {
        let mut item=template.clone();item.spec.address=pool.address;item.code_hash=pool.code_hash;item.oracle=pool.oracle;item.oracle_code_hash=pool.oracle_code_hash;
        config.pools.insert(format!("bulk-{index}"),item);
    }
    config.rpc=Default::default();config.rpc.read_optimizations.batch_log_hashes=true;
    let multicall:Address="0x0000000000000000000000000000000000000900".parse().unwrap();
    let code:Bytes=include_str!("fixtures/multicall3-runtime.hex").trim().parse().unwrap();
    local.rpc("anvil_setCode",json!([multicall,code])).await;
    config.rpc.multicall3=Some(barterlend_state::Multicall3Config{address:multicall,code_hash:keccak256(&code),..barterlend_state::Multicall3Config::canonical()});
    config.poll_ms=1000;config.queue_capacity=1000;config.v3.fee_tiers=vec![3000];config.v3.intermediates.clear();
    config.v3.limits.max_candidates=1;config.v3.limits.max_quote_calls=1;
    config.v3.limits.max_age_blocks=200;config.v3.limits.quote_ttl_seconds=60;config.planner.ttl_seconds=60;
    config.planner.max_candidates=1;config.planner.partial_divisors.clear();
    config.sync.current_state=Some(Default::default());config.sync.reconcile_user_page_size=128;
    let before=reader.head(Commitment::Latest).await.unwrap();
    let native_before:U256=serde_json::from_value(local.rpc("eth_getBalance",json!([seed.base.operator,"latest"])).await).unwrap();
    let nonce_before=quantity_u64(&local.rpc("eth_getTransactionCount",json!([seed.base.operator,"latest"])).await).unwrap();assert_eq!(nonce_before,0);
    let asset_before=reader.contract_call(seed.base.asset,&BulkToken::balanceOfCall{account:seed.base.executor},&before).await.unwrap();
    let health=Arc::new(RwLock::new(None));let observed=health.clone();let (stop_tx,stop_rx)=tokio::sync::oneshot::channel::<()>();
    let signer:PrivateKeySigner=LOCAL_DEV_KEY.parse().unwrap();assert_eq!(signer.address(),seed.base.operator);
    let started=Instant::now();
    let bot=tokio::spawn(async move {run_until(config,RunOptions{live:true,cycles:0},Some(signer),observed,async {let _=stop_rx.await;Ok(())}).await});
    let mut initial_ms=None;let mut first_ms=None;let mut observations=Vec::new();let mut completed=false;
    while started.elapsed()<Duration::from_secs(1130) && !bot.is_finished() {
        if let Some(h)=health.read().unwrap().as_ref() {
            if initial_ms.is_none() && h.pools.len()==seed.credit_pools.len() && h.pools.iter().all(|p|p.verified && p.debt_positive.is_some_and(|n|n>=seed.positions_per_market*2-seed.eligible_per_market*2)) {initial_ms=Some(started.elapsed().as_millis());}
            if h.counters.finalized_liquidations>0 && first_ms.is_none(){first_ms=Some(started.elapsed().as_millis());}
            observations.push(json!({"elapsed_ms":started.elapsed().as_millis(),"finalized":h.counters.finalized_liquidations,"queue":h.queue_len,"pools":h.pools}));
            if h.counters.finalized_liquidations==expected as u64 && h.tx.nonce.is_none(){completed=true;break;}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let _=stop_tx.send(());let report=tokio::time::timeout(Duration::from_secs(25),bot).await.expect("shutdown exceeded limit").unwrap().unwrap();
    let after=reader.head(Commitment::Latest).await.unwrap();
    let native_after:U256=serde_json::from_value(local.rpc("eth_getBalance",json!([seed.base.operator,"latest"])).await).unwrap();
    let nonce_after=quantity_u64(&local.rpc("eth_getTransactionCount",json!([seed.base.operator,"latest"])).await).unwrap();
    let asset_after=reader.contract_call(seed.base.asset,&BulkToken::balanceOfCall{account:seed.base.executor},&after).await.unwrap();
    let markets=[seed.base.market0.parse::<u64>().unwrap(),seed.market1.parse::<u64>().unwrap()];
    let mut residual_errors=0;
    for (i,pool) in seed.credit_pools.iter().enumerate(){for(m,market)in markets.iter().enumerate(){for j in 0..=seed.eligible_per_market {
        let borrower=Address::from_word(U256::from(0x100000+i*1000+m*500+j).into());
        let debt=reader.contract_call(pool.address,&CreditPool::debtOfCall{market:*market,account:borrower},&after).await.unwrap();
        if (j<seed.eligible_per_market && debt!=0)||(j==seed.eligible_per_market && debt!=100*WAD){residual_errors+=1;}
    }}}
    let mut usage=std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes one initialized rusage when it succeeds.
    let rss=if unsafe{libc::getrusage(libc::RUSAGE_SELF,usage.as_mut_ptr())}==0 {let u=unsafe{usage.assume_init()};u.ru_maxrss as u64 * if cfg!(target_os="macos"){1}else{1024}}else{0};
    let passed=completed&&report.finalized_liquidations==expected as u64&&!report.active_pending&&nonce_after==expected as u64&&native_before-native_after==report.native_spent&&asset_after>asset_before&&residual_errors==0;
    let result=json!({"passed":passed,"positions":total,"pools":seed.credit_pools.len(),"markets_per_pool":2,"target_liquidations":expected,"finalized":report.finalized_liquidations,"nonce_delta":nonce_after-nonce_before,"initial_census_ms":initial_ms,"first_finalized_ms":first_ms,"elapsed_ms":started.elapsed().as_millis(),"blocks":after.number-before.number,"block_ms":300,"finality_slots_per_epoch":1,"signed_native_charge":report.native_spent,"actual_native_spent":native_before-native_after,"asset_gain":asset_after-asset_before,"residual_errors":residual_errors,"peak_rss_bytes_bot_and_assertions":rss,"observations":observations});
    std::fs::write(std::env::var("BARTERLEND_BULK_REPORT").unwrap(),serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(),0);assert!(passed,"inspect bulk-result.json");
}
