//! Real SDK, planner, quoter, executor, RAM journal and receipt flow on a fresh Anvil.
use std::{collections::{BTreeMap,BTreeSet},sync::Arc,time::Duration};
use alloy::{primitives::{Address,B256,Bytes,U256},sol,sol_types::{SolCall,SolEvent},signers::local::PrivateKeySigner};
use barterlend_liquidator::{app::{run,RunOptions,preflight},economics::FeePolicy,local_config::{LocalManifest,LocalPoolManifest},planner::PlannerConfig,routes::{V3Config,V3CodeHashes,RouteLimits},tx::{OutcomeKind,rpc::{validate_loopback_url,quantity_u64}}};
use barterlend_state::{*,abi::CreditPool,rpc::RpcConfig};
use serde::Deserialize;
use serde_json::{Value,json};

#[cfg(unix)]
#[path = "support/closed_output.rs"]
mod closed_output;

const WAD:u128=1_000_000_000_000_000_000;
// Public, unfunded outside isolated development nodes; never loaded by the production config.
const LOCAL_DEV_KEY:&str="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
sol! {
    interface TestToken { function mint(uint256 amount) external; function approve(address spender,uint256 amount) external returns(bool); function balanceOf(address account) external view returns(uint256); }
    interface TestOracle { function setPrice(uint256 newPrice) external; }
}
#[derive(Deserialize)]
struct Seed { chain_id:u64,genesis_hash:B256,pool:Address,asset:Address,collateral:Address,oracle:Address,factory:Address,router:Address,quoter:Address,wrapped_native:Address,executor:Address,owner:Address,operator:Address,provider_a:Address,borrower_a:Address,borrower_b:Address,market0:String,runtime_code_hashes:BTreeMap<String,B256> }
struct LocalTx { client:reqwest::Client,url:reqwest::Url,timestamp:u64 }
impl LocalTx {
    async fn rpc(&self,method:&str,params:Value)->Value {
        assert!(matches!(method,"eth_chainId"|"eth_blockNumber"|"eth_getBlockByNumber"|"eth_getTransactionReceipt"|"eth_getBalance"|"eth_getTransactionCount"|"anvil_impersonateAccount"|"anvil_mine"|"eth_sendTransaction"|"evm_setNextBlockTimestamp"|"evm_mine"));
        let response:Value=self.client.post(self.url.clone()).json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert!(response.get("error").is_none(),"local {method}: {response}"); response["result"].clone()
    }
    async fn send<C:SolCall>(&mut self,from:Address,to:Address,call:C)->Value {
        self.rpc("anvil_impersonateAccount",json!([from])).await;
        let at=self.rpc("eth_getBlockByNumber",json!(["latest",false])).await;
        self.timestamp=self.timestamp.max(quantity_u64(&at["timestamp"]).unwrap()+1);
        self.rpc("evm_setNextBlockTimestamp",json!([self.timestamp])).await; self.timestamp+=1;
        let hash=self.rpc("eth_sendTransaction",json!([{"from":from,"to":to,"data":Bytes::from(call.abi_encode()),"gas":"0x989680","gasPrice":"0x77359400"}])).await;
        for _ in 0..100 { let receipt=self.rpc("eth_getTransactionReceipt",json!([hash])).await; if !receipt.is_null() { assert_eq!(receipt["status"],"0x1","test setup reverted: {receipt}"); return receipt; } tokio::time::sleep(Duration::from_millis(10)).await; }
        panic!("test setup transaction did not mine");
    }
}
fn advance_finality_after_nonce(local:&LocalTx,operator:Address,next_nonce:u64)->tokio::task::JoinHandle<()> {
    let miner=LocalTx{client:local.client.clone(),url:local.url.clone(),timestamp:0};
    tokio::spawn(async move {
        for _ in 0..1500 {
            let nonce=quantity_u64(&miner.rpc("eth_getTransactionCount",json!([operator,"latest"])).await).unwrap();
            if nonce>=next_nonce { miner.rpc("anvil_mine",json!(["0x40","0x0"])).await; return; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("expected local signed transaction was not mined before finality timeout");
    })
}
async fn open(local:&mut LocalTx,seed:&Seed,borrower:Address,market:u64) -> Value {
    local.send(borrower,seed.collateral,TestToken::mintCall{amount:U256::from(400*WAD)}).await;
    local.send(borrower,seed.collateral,TestToken::approveCall{spender:seed.pool,amount:U256::from(400*WAD)}).await;
    local.send(borrower,seed.pool,CreditPool::depositCollateralCall{account:borrower,market,amount:400*WAD}).await;
    local.send(borrower,seed.pool,CreditPool::placeBorrowOrderCall{account:borrower,market,APR:U256::from(1000),amount:100*WAD}).await;
    local.send(seed.provider_a,seed.asset,TestToken::mintCall{amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.asset,TestToken::approveCall{spender:seed.pool,amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.pool,CreditPool::depositCall{recipient:seed.provider_a,amount:100*WAD}).await;
    local.send(seed.provider_a,seed.pool,CreditPool::provideExactInCall{account:seed.provider_a,market,assetsAmount:100*WAD,minLoanAmount:0,maxCrossedOrders:10}).await
}

#[tokio::test]
#[ignore="requires fresh isolated seeded local Anvil; tests/runners/test-local-bot.sh runs it"]
async fn actual_local_bot_detects_provided_and_finalizes_profit_with_restart() {
    let url=std::env::var("BARTERLEND_LOCAL_RPC_URL").expect("explicit local RPC required");
    let url=validate_loopback_url(&url).unwrap();
    let seed:Seed=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").expect("seed path required")).unwrap()).unwrap();
    let mut local=LocalTx{client:reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(),url:url.clone(),timestamp:2};
    assert_eq!(local.rpc("eth_chainId",json!([])).await,"0x7a69");
    assert_eq!(local.rpc("eth_blockNumber",json!([])).await,"0x0","must use a new exclusive node");
    let genesis=local.rpc("eth_getBlockByNumber",json!(["0x0",false])).await;
    assert_eq!(serde_json::from_value::<B256>(genesis["hash"].clone()).unwrap(),seed.genesis_hash);
    let reader=Arc::new(HttpReader::new(url.as_str(),RpcConfig{min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap(); assert_eq!(chain.chain_id,seed.chain_id); assert_eq!(chain.genesis_hash,seed.genesis_hash);
    let at=reader.head(Commitment::Latest).await.unwrap();
    let oracle_hash=alloy::primitives::keccak256(reader.get_code(seed.oracle,&at).await.unwrap());
    let directory=tempfile::tempdir().unwrap();
    let spec=PoolSpec{address:seed.pool,deployment_block:0,expected_version:5,deployment_block_hash:Some(seed.genesis_hash)};
    let manifest=LocalManifest{ discovery: None, v4: None, kyber: None, native_price_feeds: Vec::new(), rpc: barterlend_liquidator::local_config::RpcPolicy { requests_per_second:1000,..Default::default() }, sync:Default::default(), rpc_fallback_urls: Vec::new(), recovery: Default::default(), cancellation_retry_seconds: 30, health_listen: None, sdk_history_chunks_per_step: 8,schema:1,profile:"local".into(),rpc_url:url.to_string(),chain,executor:seed.executor,executor_code_hash:seed.runtime_code_hashes["executor"],owner:seed.owner,operator:seed.operator,
        pools:BTreeMap::from([("local-pool".into(),LocalPoolManifest{spec,code_hash:seed.runtime_code_hashes["pool"],asset:TokenDescriptor{address:seed.asset,decimals:18},collateral:TokenDescriptor{address:seed.collateral,decimals:18},oracle:seed.oracle,oracle_code_hash:oracle_hash})]),
        v3:V3Config{chain,factory:seed.factory,router:seed.router,quoter:seed.quoter,wrapped_native:seed.wrapped_native,code_hashes:V3CodeHashes{factory:seed.runtime_code_hashes["factory"],router:seed.runtime_code_hashes["router"],quoter:seed.runtime_code_hashes["quoter"],wrapped_native:seed.runtime_code_hashes["wrapped_native"]},allowed_tokens:BTreeSet::from([seed.asset,seed.collateral,seed.wrapped_native]),intermediates:vec![seed.wrapped_native],fee_tiers:vec![3000],pool_code_hashes:BTreeMap::new(),limits:RouteLimits{quote_ttl_seconds:30,max_age_blocks:100,max_candidates:16,max_quote_calls:16,slippage_bps:100}},
        fees:FeePolicy{gas_margin_bps:1500,max_gas_limit:2_000_000,max_gas_price:U256::from(10_000_000_000u64),max_fee_native:U256::from(WAD/10),native_reserve:U256::from(WAD/100)},planner:PlannerConfig::default(),additional_profit_asset:U256::ZERO,max_total_native_spend:U256::from(WAD),max_attempts_per_nonce:4,max_paid_failures_per_state:3,sdk_log_chunk_size:100,poll_ms:100,queue_capacity:100,cooldown_seconds:1};
    preflight(&manifest,&reader).await.unwrap();
    let mut wrong=manifest.clone();wrong.executor_code_hash=B256::from([1;32]);assert!(preflight(&wrong,&reader).await.is_err());
    // Establish persistent debt=0 history first, then fill the already known account through Provided alone.
    let market:u64=seed.market0.parse().unwrap();
    local.send(seed.borrower_a,seed.collateral,TestToken::mintCall{amount:U256::from(400*WAD)}).await;
    local.send(seed.borrower_a,seed.collateral,TestToken::approveCall{spender:seed.pool,amount:U256::from(400*WAD)}).await;
    local.send(seed.borrower_a,seed.pool,CreditPool::depositCollateralCall{account:seed.borrower_a,market,amount:400*WAD}).await;
    local.send(seed.borrower_a,seed.pool,CreditPool::placeBorrowOrderCall{account:seed.borrower_a,market,APR:U256::from(1000),amount:100*WAD}).await;
    let report=run(manifest.clone(),RunOptions{live:false,cycles:3},None).await.unwrap();assert_eq!(report.dry_run_plans,0);
    let key=PositionKey{pool:seed.pool,market,borrower:seed.borrower_a};
    let head=reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:key.borrower},&head).await.unwrap(),0);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(),0);
    local.send(seed.provider_a,seed.asset,TestToken::mintCall{amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.asset,TestToken::approveCall{spender:seed.pool,amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.pool,CreditPool::depositCall{recipient:seed.provider_a,amount:100*WAD}).await;
    let receipt=local.send(seed.provider_a,seed.pool,CreditPool::provideExactInCall{account:seed.provider_a,market,assetsAmount:100*WAD,minLoanAmount:0,maxCrossedOrders:10}).await;
    let logs=receipt["logs"].as_array().unwrap();
    assert!(logs.iter().any(|l|l["address"].as_str().unwrap().eq_ignore_ascii_case(&seed.pool.to_string())&&l["topics"][0]==json!(CreditPool::Provided::SIGNATURE_HASH)));
    assert!(!logs.iter().any(|l|l["address"].as_str().unwrap().eq_ignore_ascii_case(&seed.pool.to_string())&&l["topics"][0]==json!(CreditPool::Borrowed::SIGNATURE_HASH)));
    local.send(seed.borrower_a,seed.oracle,TestOracle::setPriceCall{newPrice:U256::from(WAD/2)}).await;
    let dry=run(manifest.clone(),RunOptions{live:false,cycles:3},None).await.unwrap();assert!(dry.dry_run_plans>=1);
    let current=reader.head(Commitment::Latest).await.unwrap();
    assert!(reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_a},&current).await.unwrap()>0,"keyless dry-run must not liquidate");
    #[cfg(unix)] {
        // The actual CLI must still validate plans and persist health with both output peers closed.
        let cli_manifest=manifest.clone();
        let cli_path=directory.path().join("closed-output-manifest.json");
        std::fs::write(&cli_path,serde_json::to_vec(&cli_manifest).unwrap()).unwrap();
        let mut command=std::process::Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"));
        command.arg("--manifest").arg(&cli_path).arg("--cycles").arg("2")
            .env_remove("BL_LIQUIDATOR_PRIVATE_KEY");
        closed_output::disconnect_output_peers(&mut command);
        assert!(command.status().unwrap().success(),"closed output must not interrupt the actual app");
        assert!(!directory.path().join("dry-run").exists(),"CLI must not create runtime files");
        let after_cli=reader.head(Commitment::Latest).await.unwrap();assert_eq!(after_cli,current);
        assert_eq!(reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_a},&after_cli).await.unwrap(),
            reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_a},&current).await.unwrap());
    }
    let asset_before=reader.contract_call(seed.asset,&TestToken::balanceOfCall{account:seed.executor},&current).await.unwrap();
    let native_before:U256=serde_json::from_value(local.rpc("eth_getBalance",json!([seed.operator,"latest"])).await).unwrap();
    local.rpc("evm_setNextBlockTimestamp",json!([current.timestamp+1])).await;
    let signer:PrivateKeySigner=LOCAL_DEV_KEY.parse().unwrap();assert_eq!(signer.address(),seed.operator);
    let miner=advance_finality_after_nonce(&local,seed.operator,1);
    let live=run(manifest.clone(),RunOptions{live:true,cycles:30},Some(signer.clone())).await.unwrap();
    miner.await.unwrap();
    assert_eq!(live.finalized_liquidations,1,"expected successful finalized liquidation");assert!(!live.active_pending);
    let after=reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_a},&after).await.unwrap(),0);
    let gain=reader.contract_call(seed.asset,&TestToken::balanceOfCall{account:seed.executor},&after).await.unwrap()-asset_before;
    let native_after:U256=serde_json::from_value(local.rpc("eth_getBalance",json!([seed.operator,"latest"])).await).unwrap();
    let outcome=live.last_outcome.as_ref().unwrap();
    assert_eq!(outcome.kind,OutcomeKind::Liquidated);assert_eq!(outcome.asset_gain,Some(gain));
    assert_eq!(outcome.native_charge,native_before-native_after,"Monad-mode charges signed gas limit");
    assert!(gain>outcome.native_charge);assert_eq!(outcome.net_asset,None);
    let restart=run(manifest.clone(),RunOptions{live:true,cycles:3},Some(signer.clone())).await.unwrap();assert_eq!(restart.finalized_liquidations,0);assert!(!restart.active_pending);
    // Capped close drains collateral and socializes actual bad debt, using a new process session.
    local.send(seed.borrower_b,seed.oracle,TestOracle::setPriceCall{newPrice:U256::from(WAD)}).await;
    open(&mut local,&seed,seed.borrower_b,market).await;
    local.send(seed.borrower_b,seed.oracle,TestOracle::setPriceCall{newPrice:U256::from(WAD/10)}).await;
    let at=reader.head(Commitment::Latest).await.unwrap();
    let capped_debt_before=reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_b},&at).await.unwrap();
    let lender_loan_before=reader.contract_call(seed.pool,&CreditPool::loanOfCall{market,account:seed.provider_a},&at).await.unwrap();
    local.rpc("evm_setNextBlockTimestamp",json!([at.timestamp+1])).await;
    let miner=advance_finality_after_nonce(&local,seed.operator,2);
    let capped=run(manifest.clone(),RunOptions{live:true,cycles:30},Some(signer)).await.unwrap();miner.await.unwrap();assert_eq!(capped.finalized_liquidations,1);assert!(!capped.active_pending);
    let after=reader.head(Commitment::Latest).await.unwrap();assert_eq!(reader.contract_call(seed.pool,&CreditPool::debtOfCall{market,account:seed.borrower_b},&after).await.unwrap(),0);assert_eq!(reader.contract_call(seed.pool,&CreditPool::collateralOfCall{market,account:seed.borrower_b},&after).await.unwrap(),0);
    let capped_outcome=capped.last_outcome.as_ref().unwrap();
    assert_eq!(capped_outcome.kind,OutcomeKind::Liquidated);
    let bad_log=capped_outcome.receipt.logs.iter().find(|log|log.address==seed.pool&&log.topics.first()==Some(&CreditPool::BadDebtIncurred::SIGNATURE_HASH)).expect("capped collateral exhaustion must incur actual protocol bad debt");
    let bad=CreditPool::BadDebtIncurred::decode_raw_log(bad_log.topics.iter().copied(),&bad_log.data).unwrap();
    assert_eq!(bad.account,seed.borrower_b);assert_eq!(bad.market,market);assert!(bad.amount>0);
    let liquidated_log=capped_outcome.receipt.logs.iter().find(|log|log.address==seed.pool&&log.topics.first()==Some(&CreditPool::Liquidated::SIGNATURE_HASH)).unwrap();
    let liquidated=CreditPool::Liquidated::decode_raw_log(liquidated_log.topics.iter().copied(),&liquidated_log.data).unwrap();
    assert_eq!(liquidated.account,seed.borrower_b);assert_eq!(liquidated.liquidator,seed.executor);assert_eq!(liquidated.collateralSeized,400*WAD);
    assert_eq!(liquidated.debtRepaid+bad.amount,capped_debt_before,"real debt reduction must consist of actual repayment and socialized bad debt");
    assert!(reader.contract_call(seed.pool,&CreditPool::loanOfCall{market,account:seed.provider_a},&after).await.unwrap()<lender_loan_before,"real lender loan value must absorb the bad debt");
    if let Ok(output)=std::env::var("BARTERLEND_LOCAL_CONFIG_OUTPUT") { std::fs::write(output,serde_json::to_vec_pretty(&manifest).unwrap()).unwrap(); }
}
