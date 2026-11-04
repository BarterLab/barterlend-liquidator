//! Hot factory discovery and a complete signed liquidation on an isolated Anvil.
#![allow(dead_code,unused_imports)]
include!("support/app_local_fixture.rs");
use std::process::{Command,Stdio};
use barterlend_liquidator::{app::{run_until,RunOptions},catalog::{CatalogCursor,DiscoveryConfig},configuration};
use barterlend_state::factory::FactorySpec;
use alloy::signers::local::PrivateKeySigner;
sol! {
    function createPool(bytes initCode) external returns(address);
    function setVerifiedToken(address token,bool enabled) external;
}
#[derive(Deserialize)]
struct FactorySeed {
    #[serde(flatten)] base:Seed,
    lending_factory:Address,
    lending_factory_code_hash:B256,
}
struct Node(std::process::Child);
impl Drop for Node {fn drop(&mut self){let _=self.0.kill();let _=self.0.wait();}}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="fresh loopback Anvil; no remote RPC, external wallets or solc required"]
async fn factory_pools_appear_after_startup_and_liquidate_without_set_pool() {
    let root=Path::new(env!("CARGO_MANIFEST_DIR"));let directory=tempfile::tempdir().unwrap();
    let seed:FactorySeed=serde_json::from_slice(&std::fs::read(root.join("tests/fixtures/solidity/generated/manifest.json")).unwrap()).unwrap();
    let port=TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();let url=format!("http://127.0.0.1:{port}");
    let _node=Node(Command::new(std::env::var("ANVIL").unwrap_or("anvil".into()))
        .args(["--silent","--host","127.0.0.1","--port",&port.to_string(),"--network","monad","--chain-id","31337","--block-time","0.3","--slots-in-an-epoch","1","--load-state"])
        .arg(root.join("tests/fixtures/solidity/generated/anvil-state.json"))
        .stdout(Stdio::null()).stderr(std::fs::File::create(directory.path().join("anvil.log")).unwrap()).spawn().unwrap());
    let client=reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).build().unwrap();
    for attempt in 0..100 {
        if client.post(&url).json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]})).send().await.is_ok(){break;}
        assert!(attempt<99,"Anvil startup timed out");tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut local=LocalTx {client,url:validate_loopback_url(&url).unwrap(),timestamp:2};
    let reader=Arc::new(HttpReader::new(&url,RpcConfig {min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.chain_id,31337);assert_eq!(chain.genesis_hash,seed.base.genesis_hash);
    let at=reader.head(Commitment::Finalized).await.unwrap();
    let mut config=manifest(&seed.base,url.clone(),chain,keccak256(reader.get_code(seed.base.oracle,&at).await.unwrap()),directory.path());
    config.discovery=Some(DiscoveryConfig {factories:vec![FactorySpec {address:seed.lending_factory,code_hash:seed.lending_factory_code_hash,previous_factory:Address::ZERO}],poll_seconds:5,..Default::default()});
    config.pools.clear();config.rpc.requests_per_second=15;config.poll_ms=1000;
    config.v3.limits.quote_ttl_seconds=60;config.v3.limits.max_age_blocks=200;config.validate().unwrap();
    let health=Arc::new(std::sync::RwLock::new(None));let (stop_tx,stop_rx)=tokio::sync::oneshot::channel();
    let signer:PrivateKeySigner=LOCAL_DEV_KEY.parse().unwrap();
    let mut app=AppTask(Some(tokio::spawn(run_until(config.clone(),RunOptions {live:true,cycles:0},Some(signer),health.clone(),
        async {stop_rx.await.map_err(|_|std::io::Error::other("test shutdown dropped"))}))));
    tokio::time::timeout(Duration::from_secs(15),async {
        while health.read().unwrap().is_none() {tokio::time::sleep(Duration::from_millis(20)).await;}
    }).await.unwrap();
    assert!(health.read().unwrap().as_ref().unwrap().pools.is_empty());
    // Only the factory receives the create transaction. The executor is never reconfigured for this pool.
    let nonce=quantity_u64(&local.rpc("eth_getTransactionCount",json!([seed.lending_factory,"latest"])).await).unwrap();
    let first=seed.lending_factory.create(nonce);
    let init_code:Bytes=std::fs::read_to_string(root.join("tests/fixtures/solidity/generated/new-pool-initcode.hex")).unwrap().trim().parse().unwrap();
    local.send(seed.base.owner,seed.lending_factory,createPoolCall {initCode:init_code.clone()}).await;
    let creation=reader.head(Commitment::Latest).await.unwrap();
    let market=reader.contract_call(first,&CreditPool::getActiveMarketsCall {},&creation).await.unwrap()[0];
    open_fault_position(&mut local,&seed.base,first,seed.base.borrower_a,market,400*WAD).await;
    let at=reader.head(Commitment::Latest).await.unwrap();
    let spec=PoolSpec {address:first,deployment_block:0,expected_version:5,deployment_block_hash:Some(chain.genesis_hash)};
    let described=reader.describe_pool(&spec,&at).await.unwrap();
    local.send(seed.base.borrower_a,described.oracle,TestOracle::setPriceCall {newPrice:U256::from(WAD/2)}).await;
    local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
    tokio::time::timeout(Duration::from_secs(120),async {
        loop {
            if quantity_u64(&local.rpc("eth_getTransactionCount",json!([seed.base.operator,"latest"])).await).unwrap()>=1 {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
        loop {
            let at=reader.head(Commitment::Finalized).await.unwrap();
            if reader.contract_call(first,&CreditPool::debtOfCall {market,account:seed.base.borrower_a},&at).await.unwrap()==0 {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Let the app reconcile the finalized financial receipt, not merely the debt getter.
        while health.read().unwrap().as_ref().unwrap().counters.finalized_liquidations==0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.unwrap();
    stop_tx.send(()).unwrap();let report=app.finish().await;
    assert_eq!(report.finalized_liquidations,1);assert!(!report.active_pending);
    assert_eq!(report.health.unwrap().pools.len(),1);
    // A second creation is found incrementally, without restarting or any executor owner call.
    local.send(seed.base.owner,seed.lending_factory,createPoolCall {initCode:init_code}).await;
    // A factory-registered candidate with reverting getters cannot stall healthy candidates.
    let reverting_init:Bytes="0x6005600c60003960056000f360006000fd".parse().unwrap();
    local.send(seed.base.owner,seed.lending_factory,createPoolCall {initCode:reverting_init}).await;
    local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
    let mut cursor=CatalogCursor::default();cursor.step(&reader,&mut config).await.unwrap();
    assert_eq!(config.pools.len(),2);assert!(config.pools.values().any(|p|p.spec.address==first));
    assert!(config.pools.values().any(|p|p.spec.address==seed.lending_factory.create(nonce+1)));
    cursor.step(&reader,&mut config).await.unwrap();assert_eq!(config.pools.len(),2);
    // Token review remains an independent requirement, and pending pools resume after approval.
    local.send(seed.base.owner,seed.base.executor,setVerifiedTokenCall {token:seed.base.collateral,enabled:false}).await;
    local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
    config.pools.clear();let mut cursor=CatalogCursor::default();cursor.step(&reader,&mut config).await.unwrap();assert!(config.pools.is_empty());
    local.send(seed.base.owner,seed.base.executor,setVerifiedTokenCall {token:seed.base.collateral,enabled:true}).await;
    local.rpc("anvil_mine",json!(["0x40","0x0"])).await;
    cursor.step(&reader,&mut config).await.unwrap();assert_eq!(config.pools.len(),2);
    // Config init needs neither --pool nor --lending-factory: the executor is the trust anchor.
    let path=directory.path().join("discovered.toml");
    let initialized=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .args(["--rpc-url",&url,"config","init","--network","local","--executor",&config.executor.to_string(),"--operator",&config.operator.to_string(),"--quoter",&config.v3.quoter.to_string(),"--output"])
        .arg(&path).output().unwrap();
    assert!(initialized.status.success(),"{} {}",String::from_utf8_lossy(&initialized.stdout),String::from_utf8_lossy(&initialized.stderr));
    let initialized=configuration::load(&path,None).unwrap();assert_eq!(initialized.pools.len(),2);
    assert_eq!(initialized.discovery.unwrap().factories[0].address,seed.lending_factory);
    // Explicit --pool is a fixed filter even when the trusted factory contains more pools.
    let fixed_path=directory.path().join("fixed.toml");
    let fixed=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .args(["--rpc-url",&url,"config","init","--network","local","--executor",&config.executor.to_string(),"--operator",&config.operator.to_string(),"--quoter",&config.v3.quoter.to_string(),"--pool",&first.to_string(),"--output"])
        .arg(&fixed_path).output().unwrap();
    assert!(fixed.status.success(),"{}",String::from_utf8_lossy(&fixed.stderr));
    let mut fixed=configuration::load(&fixed_path,None).unwrap();assert!(fixed.discovery.is_none());assert_eq!(fixed.pools.len(),1);
    assert_eq!(fixed.pools.values().next().unwrap().spec.address,first);
    CatalogCursor::default().step(&reader,&mut fixed).await.unwrap();assert_eq!(fixed.pools.len(),1);
    let mixed_path=directory.path().join("mixed.toml");
    let mixed=Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"))
        .args(["--rpc-url",&url,"config","init","--network","local","--executor",&config.executor.to_string(),"--operator",&config.operator.to_string(),"--quoter",&config.v3.quoter.to_string(),"--pool",&first.to_string(),"--lending-factory",&seed.lending_factory.to_string(),"--output"])
        .arg(&mixed_path).output().unwrap();
    assert!(!mixed.status.success());assert!(!mixed_path.exists());
    assert!(String::from_utf8_lossy(&mixed.stderr).contains("cannot be used with"));
    config.pools.clear();CatalogCursor::default().step(&reader,&mut config).await.unwrap();assert_eq!(config.pools.len(),2);
    let at=reader.head(Commitment::Finalized).await.unwrap();
    config.discovery.as_mut().unwrap().factories=vec![FactorySpec {address:seed.base.factory,code_hash:keccak256(reader.get_code(seed.base.factory,&at).await.unwrap()),previous_factory:Address::ZERO}];
    assert!(barterlend_liquidator::catalog::verify_binding(&reader,&config,&at).await.is_err(),"a config must not add an executor-untrusted factory");
}
