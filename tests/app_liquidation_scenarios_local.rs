//! Whole-app liquidation scenarios on a fresh genuine CreditPool/V3 Monad-mode Anvil.
//! Setup uses public lending operations; the only oracle change is an explicit local fixture price.

use std::{collections::{BTreeMap, BTreeSet}, path::Path, sync::Arc,
    time::{Duration, Instant}};

use alloy::{primitives::{Address, B256, Bytes, U256, keccak256, aliases::{I24, U24}}, signers::local::PrivateKeySigner,
    sol, sol_types::{SolCall, SolEvent}};
use barterlend_liquidator::{app::{preflight, run, RunOptions, RunReport},
    diagnostics::HealthSnapshot,
    economics::{FeePolicy, NativeCostOracle, V3NativeCostOracle},
    local_config::{LocalManifest, LocalPoolManifest}, planner::{plan_position, PlannerConfig},
    routes::{ExactInputRequest, RouteKind, RouteLimits, V3CodeHashes, V3Config, V3RouteProvider},
    simulation::{prepare, SimulationPolicy},
    tx::{AttemptKind, Outcome, OutcomeKind,
        rpc::{quantity_u64, validate_loopback_url, LocalTxRpc}}};
use barterlend_state::{*, abi::CreditPool, rpc::RpcConfig};
use serde::Deserialize;
use serde_json::{Value, json};

const WAD: u128 = 1_000_000_000_000_000_000;
include!("support/sync_policy.rs");
// Public development key, valid only for the explicitly supplied isolated loopback fixture.
const LOCAL_DEV_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

sol! {
    interface TestToken {
        function mint(uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
    interface TestOracle {
        function setPrice(uint256 newPrice) external;
        function setPriceFraction(uint256 newPrice, uint256 denominator) external;
        function setUpdatedAt(uint256 updatedAt) external;
    }
    interface V3PoolRead { function liquidity() external view returns (uint128); }
    interface V3FactoryAdmin {
        function owner() external view returns (address);
        function enableFeeAmount(uint24 fee, int24 tickSpacing) external;
    }
    interface ExecutorOwner {
        function withdraw(address token, address recipient, uint256 amount) external;
    }
}

#[derive(Deserialize)]
struct Seed {
    chain_id: u64, genesis_hash: B256, pool: Address, asset: Address, collateral: Address,
    oracle: Address, factory: Address, router: Address, quoter: Address, wrapped_native: Address,
    executor: Address, owner: Address, operator: Address, provider_a: Address, borrower_a: Address,
    market0: String, runtime_code_hashes: BTreeMap<String, B256>,
}

struct LocalTx { client: reqwest::Client, url: reqwest::Url, timestamp: u64 }

impl LocalTx {
    async fn rpc(&self, method: &str, params: Value) -> Value {
        assert!(matches!(method, "eth_chainId" | "eth_blockNumber" | "eth_getBlockByNumber"
            | "eth_getTransactionReceipt" | "eth_getBalance" | "eth_getTransactionCount"
            | "anvil_impersonateAccount" | "anvil_mine" | "eth_sendTransaction"
            | "evm_setNextBlockTimestamp" | "evm_mine" | "evm_setAutomine" | "anvil_setBalance"));
        let response: Value = self.client.post(self.url.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert!(response.get("error").is_none(), "local {method}: {response}");
        response["result"].clone()
    }

    async fn send<C: SolCall>(&mut self, from: Address, to: Address, call: C) -> Value {
        self.rpc("anvil_impersonateAccount", json!([from])).await;
        let at = self.rpc("eth_getBlockByNumber", json!(["latest", false])).await;
        self.timestamp = self.timestamp.max(quantity_u64(&at["timestamp"]).unwrap() + 1);
        self.rpc("evm_setNextBlockTimestamp", json!([self.timestamp])).await;
        self.timestamp += 1;
        let hash = self.rpc("eth_sendTransaction", json!([{"from":from,"to":to,
            "data":Bytes::from(call.abi_encode()),"gas":"0x989680","gasPrice":"0x77359400"}])).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let receipt = self.rpc("eth_getTransactionReceipt", json!([hash])).await;
            if !receipt.is_null() { assert_eq!(receipt["status"], "0x1", "public setup reverted: {receipt}"); return receipt; }
            assert!(Instant::now() < deadline, "setup transaction did not mine");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// With no pending transactions this advances only chain time, with an explicit proof of emptiness.
    async fn mine_empty_at(&self, timestamp: u64) -> Value {
        self.rpc("evm_setNextBlockTimestamp", json!([timestamp])).await;
        self.rpc("evm_mine", json!([])).await;
        let block = self.rpc("eth_getBlockByNumber", json!(["latest", false])).await;
        assert_eq!(quantity_u64(&block["timestamp"]).unwrap(), timestamp);
        assert!(block["transactions"].as_array().unwrap().is_empty(), "time transition must be an empty block");
        block
    }
}

async fn open_provided(local: &mut LocalTx, seed: &Seed, market: u64) {
    local.send(seed.borrower_a, seed.collateral, TestToken::mintCall { amount: U256::from(400 * WAD) }).await;
    local.send(seed.borrower_a, seed.collateral, TestToken::approveCall { spender: seed.pool, amount: U256::from(400 * WAD) }).await;
    local.send(seed.borrower_a, seed.pool, CreditPool::depositCollateralCall { account: seed.borrower_a, market, amount: 400 * WAD }).await;
    local.send(seed.borrower_a, seed.pool, CreditPool::placeBorrowOrderCall { account: seed.borrower_a, market, APR: U256::from(1000), amount: 100 * WAD }).await;
    local.send(seed.provider_a, seed.asset, TestToken::mintCall { amount: U256::from(100 * WAD) }).await;
    local.send(seed.provider_a, seed.asset, TestToken::approveCall { spender: seed.pool, amount: U256::from(100 * WAD) }).await;
    local.send(seed.provider_a, seed.pool, CreditPool::depositCall { recipient: seed.provider_a, amount: 100 * WAD }).await;
    local.send(seed.provider_a, seed.pool, CreditPool::provideExactInCall { account: seed.provider_a, market,
        assetsAmount: 100 * WAD, minLoanAmount: 0, maxCrossedOrders: 10 }).await;
}

fn manifest(seed: &Seed, url: String, chain: ChainIdentity, oracle_code_hash: B256, _runtime_dir: &Path) -> LocalManifest {
    LocalManifest { discovery: None, v4: None, kyber: None, native_price_feeds: Vec::new(), rpc: barterlend_liquidator::local_config::RpcPolicy { requests_per_second: 1000, ..Default::default() }, sync: local_sync_policy(), rpc_fallback_urls: Vec::new(), recovery: Default::default(), cancellation_retry_seconds: 30, health_listen: None, sdk_history_chunks_per_step: 8, schema: 1, profile: "local".into(), rpc_url: url, chain,
        executor: seed.executor, executor_code_hash: seed.runtime_code_hashes["executor"], owner: seed.owner, operator: seed.operator,
        pools: BTreeMap::from([("local-pool".into(), LocalPoolManifest {
            spec: PoolSpec { address: seed.pool, deployment_block: 0, expected_version: 5, deployment_block_hash: Some(seed.genesis_hash) },
            code_hash: seed.runtime_code_hashes["pool"], asset: TokenDescriptor { address: seed.asset, decimals: 18 },
            collateral: TokenDescriptor { address: seed.collateral, decimals: 18 }, oracle: seed.oracle, oracle_code_hash,
        })]),
        v3: V3Config { chain, factory: seed.factory, router: seed.router, quoter: seed.quoter, wrapped_native: seed.wrapped_native,
            code_hashes: V3CodeHashes { factory: seed.runtime_code_hashes["factory"], router: seed.runtime_code_hashes["router"],
                quoter: seed.runtime_code_hashes["quoter"], wrapped_native: seed.runtime_code_hashes["wrapped_native"] },
            allowed_tokens: BTreeSet::from([seed.asset, seed.collateral, seed.wrapped_native]), intermediates: vec![seed.wrapped_native],
            fee_tiers: vec![3000], pool_code_hashes: BTreeMap::new(),
            limits: RouteLimits { quote_ttl_seconds: 30, max_age_blocks: 100, max_candidates: 16, max_quote_calls: 16, slippage_bps: 100 } },
        fees: FeePolicy { gas_margin_bps: 1500, max_gas_limit: 2_000_000, max_gas_price: U256::from(10_000_000_000u64),
            max_fee_native: U256::from(WAD / 10), native_reserve: U256::from(WAD / 100) },
        planner: PlannerConfig::default(), additional_profit_asset: U256::ZERO, max_total_native_spend: U256::from(WAD),
        max_attempts_per_nonce: 4, max_paid_failures_per_state: 3, sdk_log_chunk_size: 100, poll_ms: 100, queue_capacity: 100, cooldown_seconds: 1,
         }
}

struct Fixture { seed: Seed, local: LocalTx, reader: Arc<HttpReader>, manifest: LocalManifest, _directory: tempfile::TempDir, last_health: std::cell::RefCell<Option<HealthSnapshot>> }

impl Fixture {
    async fn fresh() -> Self {
        let url = validate_loopback_url(&std::env::var("BARTERLEND_LOCAL_RPC_URL").expect("explicit local RPC required")).unwrap();
        let seed: Seed = serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").expect("local seed path required")).unwrap()).unwrap();
        let local = LocalTx { client: reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none()).build().unwrap(), url: url.clone(), timestamp: 2 };
        assert_eq!(local.rpc("eth_chainId", json!([])).await, "0x7a69");
        assert_eq!(local.rpc("eth_blockNumber", json!([])).await, "0x0", "each scenario needs its own fresh exclusive node");
        let reader = Arc::new(HttpReader::new(url.as_str(), RpcConfig { min_request_interval: Duration::from_millis(1), ..Default::default() }).unwrap());
        let chain = reader.identity().await.unwrap(); assert_eq!(chain.chain_id, seed.chain_id); assert_eq!(chain.genesis_hash, seed.genesis_hash);
        let initial = reader.head(Commitment::Latest).await.unwrap();
        let oracle_code_hash = keccak256(reader.get_code(seed.oracle, &initial).await.unwrap());
        let directory = tempfile::tempdir().unwrap();
        let manifest = manifest(&seed, url.to_string(), chain, oracle_code_hash, directory.path());
        preflight(&manifest, &reader).await.unwrap();
        Self { seed, local, reader, manifest, _directory: directory, last_health: Default::default() }
    }

    fn key(&self) -> PositionKey { PositionKey { pool: self.seed.pool, market: self.seed.market0.parse().unwrap(), borrower: self.seed.borrower_a } }
    fn spec(&self) -> PoolSpec { self.manifest.pools["local-pool"].spec }
    fn signer(&self) -> PrivateKeySigner {
        let signer: PrivateKeySigner = LOCAL_DEV_KEY.parse().unwrap(); assert_eq!(signer.address(), self.seed.operator); signer
    }
    async fn snapshot(&self) -> StateSnapshot {
        let mut engine = StateEngine::new(self.reader.clone(), self.spec(), EngineConfig {
            commitment: Commitment::Latest, expected_identity: Some(self.manifest.chain), ..Default::default()
        }).unwrap();
        engine.bootstrap().await.unwrap()
    }
    async fn executor_asset(&self, at: &BlockRef) -> U256 {
        self.reader.contract_call(self.seed.asset, &TestToken::balanceOfCall { account: self.seed.executor }, at).await.unwrap()
    }
    async fn operator_native(&self) -> U256 {
        serde_json::from_value(self.local.rpc("eth_getBalance", json!([self.seed.operator, "latest"])).await).unwrap()
    }

    fn health(&self) -> HealthSnapshot { self.last_health.borrow().clone().expect("observed run health") }
    fn record(&self, report: &RunReport) { *self.last_health.borrow_mut()=report.health.clone(); }

    async fn assert_no_send(&self, label: &str, manifest: LocalManifest, expect_skip: bool) {
        let before = self.reader.head(Commitment::Latest).await.unwrap();
        let native = self.operator_native().await;
        let asset = self.executor_asset(&before).await;
        let key = self.key();
        let debt = self.reader.contract_call(self.seed.pool, &CreditPool::debtOfCall {
            market: key.market, account: key.borrower }, &before).await.unwrap();
        let collateral = self.reader.contract_call(self.seed.pool, &CreditPool::collateralOfCall {
            market: key.market, account: key.borrower }, &before).await.unwrap();
        let report = tokio::time::timeout(Duration::from_secs(45),
            run(manifest, RunOptions { live: true, cycles: 4 }, Some(self.signer())))
            .await.expect("bounded no-send scenario timed out").unwrap();
        self.record(&report);
        assert_eq!(report.finalized_liquidations, 0, "{label}"); assert!(!report.active_pending, "{label}");
        assert_eq!(report.dry_run_plans, 0, "{label}");
        if expect_skip { assert!(report.skips > 0, "{label}: the candidate must reach preparation and be rejected"); }
        assert_eq!(self.local.rpc("eth_getTransactionCount", json!([self.seed.operator, "latest"])).await, "0x0", "{label}");
        assert_eq!(self.local.rpc("eth_getTransactionCount", json!([self.seed.operator, "pending"])).await, "0x0", "{label}");
        assert!(report.last_outcome.is_none());
        let after = self.reader.head(Commitment::Latest).await.unwrap();
        assert_eq!(after, before, "{label}: read/quote/simulation must not mine a block");
        assert_eq!(self.operator_native().await, native, "{label}: no MON may be spent");
        assert_eq!(self.executor_asset(&after).await, asset, "{label}");
        assert_eq!(self.reader.contract_call(self.seed.pool, &CreditPool::debtOfCall {
            market: key.market, account: key.borrower }, &after).await.unwrap(), debt, "{label}");
        assert_eq!(self.reader.contract_call(self.seed.pool, &CreditPool::collateralOfCall {
            market: key.market, account: key.borrower }, &after).await.unwrap(), collateral, "{label}");
        println!("no-send acceptance passed: {label}");
    }

    async fn assert_success(&self, report: &RunReport, asset_before: U256, native_before: U256) -> Outcome {
        self.record(report);
        assert!(!report.active_pending);
        let outcome=report.last_outcome.clone().expect("finalized outcome");
        assert_eq!(outcome.kind, OutcomeKind::Liquidated); assert_eq!(outcome.attempts, vec![outcome.attempt.clone()]);
        let meta = match &outcome.attempt.kind { AttemptKind::Liquidation(meta) => meta, AttemptKind::Cancel => panic!("expected liquidation") };
        let at = self.reader.head(Commitment::Latest).await.unwrap();
        let gain = self.executor_asset(&at).await - asset_before;
        assert_eq!(outcome.asset_gain, Some(gain)); assert!(gain >= meta.min_asset_profit && meta.min_asset_profit > U256::ZERO);
        assert_eq!(native_before - self.operator_native().await, outcome.native_charge, "Monad-mode balance must bill the signed gas limit");
        assert_eq!(outcome.native_charge, outcome.attempt.envelope.native_bound().unwrap());
        let routes = Arc::new(V3RouteProvider::new(self.reader.clone(), self.manifest.v3.clone()).unwrap());
        let cost = V3NativeCostOracle::new(routes, self.seed.executor, 100).unwrap()
            .quote_cost(self.seed.asset, outcome.native_charge, &meta.as_of).await.unwrap();
        assert!(meta.min_asset_profit >= cost.cost_asset + self.manifest.additional_profit_asset,
            "actual asset guard must cover the conservative asset cost of the signed MON charge");
        assert!(gain > cost.cost_asset); assert_eq!(outcome.net_asset, None, "realised FX remains unmeasured");
        outcome
    }
}

fn risk(snapshot: &StateSnapshot, key: PositionKey) -> PositionRisk {
    match &snapshot.positions[&key].risk { RiskStatus::Ready(risk) => *risk, other => panic!("risk must be ready: {other:?}") }
}

struct AppTask(Option<tokio::task::JoinHandle<color_eyre::eyre::Result<RunReport>>>);
impl AppTask {
    async fn finish(&mut self) -> RunReport {
        let result = tokio::time::timeout(Duration::from_secs(60), self.0.as_mut().unwrap()).await
            .expect("bounded app did not stop").expect("app task panicked").expect("app run failed");
        self.0.take(); result
    }
}
impl Drop for AppTask { fn drop(&mut self) { if let Some(task) = self.0.take() { task.abort(); } } }

fn advance_finality_after_nonce(local: &LocalTx, operator: Address) -> tokio::task::JoinHandle<()> {
    let miner = LocalTx { client: local.client.clone(), url: local.url.clone(), timestamp: 0 };
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if quantity_u64(&miner.rpc("eth_getTransactionCount", json!([operator, "latest"])).await).unwrap() >= 1 {
                miner.rpc("anvil_mine", json!(["0x40", "0x0"])).await; return;
            }
            assert!(Instant::now() < deadline, "expected signed liquidation did not mine");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires its own fresh isolated seeded Monad-mode Anvil"]
async fn whole_app_rejects_unprofitable_full_and_finalizes_profitable_partial() {
    let mut fixture = Fixture::fresh().await;
    // Only the real narrow +/-6000 tick, L=200e18 direct pool is an allowed unwind route.
    // The seeded full-range 500/3000 pools remain deployed, but are deliberately outside this route policy.
    fixture.manifest.v3.fee_tiers = vec![10000]; fixture.manifest.v3.intermediates.clear();
    let key = fixture.key();
    open_provided(&mut fixture.local, &fixture.seed, key.market).await;
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle, TestOracle::setPriceCall { newPrice: U256::from(WAD / 2) }).await;
    let snapshot = fixture.snapshot().await;
    assert!(risk(&snapshot, key).eligible); let original = snapshot.positions[&key].amounts;
    assert!(original.debt > 100 * WAD); assert_eq!(original.collateral, 400 * WAD);
    let plans = plan_position(&snapshot, &key, &fixture.manifest.planner).unwrap();
    assert!(plans.len() > 2); let full = &plans[0];
    assert_eq!(full.requested_debt, original.debt); assert_eq!(full.expected.remaining_debt, 0); assert_eq!(full.expected.bad_debt, 0);
    let routes = Arc::new(V3RouteProvider::new(fixture.reader.clone(), fixture.manifest.v3.clone()).unwrap());
    let full_quotes = routes.quote_exact_input_candidates(&ExactInputRequest {
        token_in: fixture.seed.collateral, token_out: fixture.seed.asset,
        amount_in: U256::from(full.expected.seized_collateral), executor: fixture.seed.executor, at: snapshot.as_of,
    }).await.unwrap();
    assert_eq!(full_quotes.len(), 1); let full_quote = &full_quotes[0];
    assert!(full_quote.amount_out < U256::from(full.expected.actual_repay), "genuine tick-limited full unwind cannot repay the debt even before gas");
    let pool = match &full_quote.route {
        RouteKind::V3 { pools, fees, .. } => { assert_eq!(fees, &vec![10000]); assert_eq!(pools.len(), 1); pools[0] }
        RouteKind::SameToken | RouteKind::V4 { .. } | RouteKind::Kyber { .. } => panic!("this scenario requires a genuine V3 pool"),
    };
    assert_eq!(fixture.reader.contract_call(pool, &V3PoolRead::liquidityCall {}, &snapshot.as_of).await.unwrap(), 200 * WAD);
    let rpc = LocalTxRpc::new(fixture.local.url.as_str(), fixture.reader.clone()).unwrap();
    let native_cost = V3NativeCostOracle::new(routes.clone(), fixture.seed.executor, 100).unwrap();
    let policy = SimulationPolicy { max_iterations: 6, additional_profit_asset: fixture.manifest.additional_profit_asset };
    assert!(prepare(&rpc, &native_cost, full, full_quote, fixture.seed.operator, fixture.seed.executor, 0,
        fixture.manifest.chain.chain_id, &fixture.manifest.fees, &policy).await.is_err(), "full plan must be rejected by the actual execution/simulation pipeline");
    let mut accepted = None;
    for funding in plans.iter().skip(1) {
        let quotes = routes.quote_exact_input_candidates(&ExactInputRequest {
            token_in: fixture.seed.collateral, token_out: fixture.seed.asset,
            amount_in: U256::from(funding.expected.seized_collateral), executor: fixture.seed.executor, at: snapshot.as_of,
        }).await.unwrap();
        for quote in quotes {
            if let Ok(plan) = prepare(&rpc, &native_cost, funding, &quote, fixture.seed.operator, fixture.seed.executor, 0,
                fixture.manifest.chain.chain_id, &fixture.manifest.fees, &policy).await {
                accepted = Some((funding.clone(), plan)); break;
            }
        }
        if accepted.is_some() { break; }
    }
    let (partial, prepared) = accepted.expect("bounded real partial candidate should be profitable");
    assert!(partial.requested_debt < original.debt); assert!(partial.expected.remaining_debt >= snapshot.globals.min_debt_remaining);
    assert_eq!(partial.expected.bad_debt, 0); assert!(prepared.metadata().min_asset_profit > fixture.manifest.additional_profit_asset);
    assert_eq!(fixture.local.rpc("eth_getTransactionCount", json!([fixture.seed.operator, "latest"])).await, "0x0", "quotes and simulations are read-only");
    let asset_before = fixture.executor_asset(&snapshot.as_of).await; let native_before = fixture.operator_native().await;
    fixture.local.rpc("evm_setNextBlockTimestamp", json!([snapshot.as_of.timestamp + 1])).await;
    let miner = advance_finality_after_nonce(&fixture.local, fixture.seed.operator);
    let report = tokio::time::timeout(Duration::from_secs(60), run(fixture.manifest.clone(), RunOptions { live: true, cycles: 40 }, Some(fixture.signer())))
        .await.expect("partial app timed out").unwrap(); miner.await.unwrap();
    assert_eq!(report.finalized_liquidations, 1); assert!(!report.active_pending);
    let outcome = fixture.assert_success(&report, asset_before, native_before).await;
    let meta = match &outcome.attempt.kind { AttemptKind::Liquidation(meta) => meta, AttemptKind::Cancel => panic!("expected partial liquidation") };
    assert_eq!(meta.requested_debt, U256::from(partial.requested_debt)); assert!(meta.requested_debt < U256::from(original.debt));
    let after = fixture.reader.head(Commitment::Latest).await.unwrap();
    let debt = fixture.reader.contract_call(fixture.seed.pool, &CreditPool::debtOfCall { market: key.market, account: key.borrower }, &after).await.unwrap();
    assert!(debt > 0 && debt >= snapshot.globals.min_debt_remaining); assert_eq!(debt, partial.expected.remaining_debt);
    assert_eq!(fixture.reader.contract_call(pool, &V3PoolRead::liquidityCall {}, &after).await.unwrap(), 0, "actual swap must cross and exhaust the narrow initialized tick range");
    let final_snapshot = fixture.snapshot().await; assert!(!risk(&final_snapshot, key).eligible, "partial repayment should restore this borrower's health");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires its own fresh isolated seeded Monad-mode Anvil"]
async fn whole_app_maturity_transition_without_lending_events_finalizes_liquidation() {
    let mut fixture = Fixture::fresh().await; let key = fixture.key();
    open_provided(&mut fixture.local, &fixture.seed, key.market).await;
    let original = fixture.snapshot().await;
    assert!(!risk(&original, key).eligible); assert!(risk(&original, key).health_factor >= U256::from(WAD));
    let early = run(fixture.manifest.clone(), RunOptions { live: false, cycles: 3 }, None).await.unwrap();
    assert_eq!(early.dry_run_plans, 0);
    let boundary = key.market.checked_add(original.markets[&key.market].grace_period).unwrap();
    assert!(boundary > original.as_of.timestamp + 30);
    fixture.local.mine_empty_at(boundary - 22).await;
    // Refresh once BEFORE the observation interval; unchanged 1:1 price avoids a stale-oracle confounder.
    let refreshed = fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle, TestOracle::setPriceCall { newPrice: U256::from(WAD) }).await;
    assert!(refreshed["logs"].as_array().unwrap().is_empty(), "fixture oracle refresh emits no lending or oracle events");
    let interval_start = fixture.reader.head(Commitment::Latest).await.unwrap();
    assert!(interval_start.timestamp < boundary);
    let asset_before = fixture.executor_asset(&interval_start).await; let native_before = fixture.operator_native().await;
    let mut running = AppTask(Some(tokio::spawn(run(fixture.manifest.clone(), RunOptions { live: true, cycles: 120 }, Some(fixture.signer())))));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fixture.local.rpc("eth_getTransactionCount", json!([fixture.seed.operator, "pending"])).await,"0x0");
    fixture.local.rpc("evm_setAutomine",json!([false])).await;
    fixture.local.mine_empty_at(boundary-1).await;
    let just_before=fixture.snapshot().await;
    assert!(!risk(&just_before,key).eligible);
    let crossed = fixture.local.mine_empty_at(boundary).await;
    let crossed_number = quantity_u64(&crossed["number"]).unwrap();
    assert!(fixture.reader.logs(fixture.seed.pool, interval_start.number + 1, crossed_number).await.unwrap().is_empty(),
        "eligibility must change through empty blocks, without any lending log");
    let eligible=fixture.snapshot().await;
    assert!(risk(&eligible,key).eligible && risk(&eligible,key).health_factor>=U256::from(WAD));
    assert_eq!(eligible.positions[&key].amounts,original.positions[&key].amounts);
    assert_eq!(eligible.oracle,just_before.oracle);
    let deadline=Instant::now()+Duration::from_secs(20);
    while fixture.local.rpc("eth_getTransactionCount",json!([fixture.seed.operator,"pending"])).await=="0x0" {
        assert!(Instant::now()<deadline,"maturity liquidation never broadcast");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fixture.local.rpc("eth_getTransactionCount",json!([fixture.seed.operator,"latest"])).await,"0x0");
    // From this point inclusion intentionally contains the actual bot transaction; only the preceding transition was empty.
    fixture.local.rpc("evm_setNextBlockTimestamp", json!([boundary + 1])).await;
    fixture.local.rpc("evm_mine", json!([])).await;
    fixture.local.rpc("anvil_mine", json!(["0x40", "0x0"])).await;
    fixture.local.rpc("evm_setAutomine", json!([true])).await;
    let report = running.finish().await; assert_eq!(report.finalized_liquidations, 1); assert!(!report.active_pending);
    let outcome = fixture.assert_success(&report, asset_before, native_before).await;
    assert!(outcome.receipt.succeeded);
    let meta=match &outcome.attempt.kind { AttemptKind::Liquidation(m)=>m,_=>panic!("expected maturity liquidation") };
    assert!(meta.as_of.timestamp>=boundary);
    let after = fixture.reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(fixture.reader.contract_call(fixture.seed.pool, &CreditPool::debtOfCall { market: key.market, account: key.borrower }, &after).await.unwrap(), 0);
    assert!(fixture.reader.contract_call(fixture.seed.pool, &CreditPool::collateralOfCall { market: key.market, account: key.borrower }, &after).await.unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires its own fresh isolated seeded Monad-mode Anvil"]
async fn whole_app_healthy_invalid_and_stale_oracle_never_sign_then_fresh_price_recovers() {
    let mut fixture = Fixture::fresh().await; let key = fixture.key();
    open_provided(&mut fixture.local, &fixture.seed, key.market).await;
    let healthy = fixture.snapshot().await;
    assert!(!risk(&healthy, key).eligible);
    fixture.assert_no_send("healthy debt-bearing position", fixture.manifest.clone(), false).await;
    assert_eq!(fixture.health().pools[0].eligible, Some(0));

    let zero = fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceCall { newPrice: U256::ZERO }).await;
    assert!(zero["logs"].as_array().unwrap().is_empty(), "oracle-only transition has no lending event");
    fixture.assert_no_send("zero oracle numerator", fixture.manifest.clone(), false).await;
    assert!(matches!(fixture.snapshot().await.positions[&key].risk,
        RiskStatus::Unavailable(RiskFailure::Oracle(OracleFailure::ZeroNumerator))));
    assert!(!fixture.health().pools[0].actionable);

    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceFractionCall { newPrice: U256::from(WAD / 2), denominator: U256::ZERO }).await;
    fixture.assert_no_send("zero oracle denominator", fixture.manifest.clone(), false).await;
    assert!(matches!(fixture.snapshot().await.positions[&key].risk,
        RiskStatus::Unavailable(RiskFailure::Oracle(OracleFailure::ZeroDenominator))));

    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceFractionCall { newPrice: U256::from(WAD / 2), denominator: U256::from(WAD) }).await;
    let valid = fixture.snapshot().await; assert!(risk(&valid, key).eligible);
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setUpdatedAtCall { updatedAt: U256::from(valid.as_of.timestamp + 1_000) }).await;
    let future = fixture.snapshot().await;
    assert!(risk(&future, key).eligible, "SDK keeps protocol-exact future timestamp acceptance");
    fixture.assert_no_send("future oracle timestamp despite eligible protocol risk", fixture.manifest.clone(), false).await;
    assert!(!fixture.health().pools[0].actionable);
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceCall { newPrice: U256::from(WAD / 2) }).await;
    let valid = fixture.snapshot().await;
    let price = match valid.oracle { OracleStatus::Ready(price) => price, other => panic!("{other:?}") };
    let expiry = u64::try_from(price.updated_at).unwrap() + valid.globals.stale_window;
    assert!(expiry < key.market, "oracle stale boundary must remain before maturity");
    fixture.local.mine_empty_at(expiry).await;
    fixture.assert_no_send("oracle becomes stale at exact expiry through an empty block", fixture.manifest.clone(), false).await;
    assert!(matches!(fixture.snapshot().await.positions[&key].risk,
        RiskStatus::Unavailable(RiskFailure::Oracle(OracleFailure::Stale { .. }))));
    assert!(!fixture.health().pools[0].actionable);

    let refresh = fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceCall { newPrice: U256::from(WAD / 2) }).await;
    assert!(refresh["logs"].as_array().unwrap().is_empty());
    let current = fixture.reader.head(Commitment::Latest).await.unwrap();
    let native_before = fixture.operator_native().await; let asset_before = fixture.executor_asset(&current).await;
    fixture.local.rpc("evm_setNextBlockTimestamp", json!([current.timestamp + 1])).await;
    let miner = advance_finality_after_nonce(&fixture.local, fixture.seed.operator);
    let report = tokio::time::timeout(Duration::from_secs(60), run(fixture.manifest.clone(),
        RunOptions { live: true, cycles: 30 }, Some(fixture.signer()))).await.unwrap().unwrap();
    miner.await.unwrap(); assert_eq!(report.finalized_liquidations, 1); assert!(!report.active_pending);
    fixture.assert_success(&report, asset_before, native_before).await;
    let after = fixture.reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(fixture.reader.contract_call(fixture.seed.pool, &CreditPool::debtOfCall {
        market: key.market, account: key.borrower }, &after).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires its own fresh isolated seeded Monad-mode Anvil"]
async fn whole_app_route_native_profit_and_fee_guards_never_sign_or_spend() {
    let mut fixture = Fixture::fresh().await; let key = fixture.key();
    open_provided(&mut fixture.local, &fixture.seed, key.market).await;
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceCall { newPrice: U256::from(WAD / 2) }).await;
    assert!(risk(&fixture.snapshot().await, key).execution_ready);

    // An actual enabled factory fee with no deployed pair is different from invalid configuration.
    let at = fixture.reader.head(Commitment::Latest).await.unwrap();
    let factory_owner = fixture.reader.contract_call(fixture.seed.factory,
        &V3FactoryAdmin::ownerCall {}, &at).await.unwrap();
    fixture.local.rpc("anvil_setBalance", json!([factory_owner, U256::from(100 * WAD)])).await;
    fixture.local.send(factory_owner, fixture.seed.factory,
        V3FactoryAdmin::enableFeeAmountCall { fee: U24::from(100), tickSpacing: I24::try_from(1).unwrap() }).await;
    let mut no_route = fixture.manifest.clone(); no_route.v3.fee_tiers = vec![100]; no_route.v3.intermediates.clear();
    fixture.assert_no_send("enabled V3 fee without an unwind pool", no_route, true).await;
    assert_eq!(fixture.health().routes[0].outcome, "quote_unavailable");

    // 500 collateral/asset exists, but 500 asset/WMON does not. An unwind quote alone cannot price MON.
    let mut no_native_quote = fixture.manifest.clone();
    no_native_quote.v3.fee_tiers = vec![500]; no_native_quote.v3.intermediates.clear();
    fixture.assert_no_send("unwind exists but native cost conversion is unavailable", no_native_quote, true).await;
    assert!(fixture.health().routes[0].successful_quotes.unwrap() > 0);
    assert!(fixture.health().routes[0].validated_at.is_none());

    let mut loss = fixture.manifest.clone(); loss.additional_profit_asset = U256::from(1000 * WAD);
    fixture.assert_no_send("every candidate fails required asset profit", loss, true).await;
    let mut gas_price = fixture.manifest.clone(); gas_price.fees.max_gas_price = U256::from(1);
    fixture.assert_no_send("network gas price exceeds configured cap", gas_price, true).await;
    let mut gas_limit = fixture.manifest.clone(); gas_limit.fees.max_gas_limit = 21_000;
    fixture.assert_no_send("execution cannot fit configured gas limit", gas_limit, true).await;
    let mut fee_limit = fixture.manifest.clone(); fee_limit.fees.max_fee_native = U256::from(1);
    fixture.assert_no_send("native transaction fee cannot cover intrinsic gas", fee_limit, true).await;

    let native = fixture.operator_native().await;
    fixture.local.rpc("anvil_setBalance", json!([fixture.seed.operator, "0x0"])).await;
    fixture.assert_no_send("operator has zero MON", fixture.manifest.clone(), true).await;
    assert_eq!(fixture.operator_native().await, U256::ZERO, "zero funding must remain unchanged on the origin");
    let stopped = fixture.health();
    assert_eq!(stopped.status, "stopped");
    assert_eq!(stopped.native_balance.status, "unknown");
    assert!(stopped.native_balance.raw.is_none());
    assert_eq!(stopped.native_balance.reason.as_deref(), Some("shutdown_not_refreshed"));
    fixture.local.rpc("anvil_setBalance", json!([fixture.seed.operator, native])).await;

    // Total budget is checked after successful simulation but before reservation/signature.
    let mut total = fixture.manifest.clone();
    total.max_total_native_spend = total.fees.max_fee_native; total.fees.native_reserve = total.max_total_native_spend;
    let before = fixture.reader.head(Commitment::Latest).await.unwrap();
    let report = run(total, RunOptions { live: true, cycles: 4 }, Some(fixture.signer())).await.unwrap();
    fixture.record(&report);
    assert!(!report.active_pending && report.finalized_liquidations==0 && report.skips>0,
        "exhausted total budget must reject the prepared candidate while preserving the service");
    assert!(fixture.health().routes.iter().any(|route|route.validated_at.is_some()),
        "budget refusal must follow a genuine successful simulation");
    assert!(report.last_outcome.is_none());
    assert_eq!(fixture.local.rpc("eth_getTransactionCount", json!([fixture.seed.operator, "pending"])).await, "0x0");
    assert_eq!(fixture.operator_native().await, native);
    assert_eq!(fixture.reader.head(Commitment::Latest).await.unwrap(), before);
    assert_eq!(fixture.health().status, "stopped", "bounded run stops gracefully after the budget refusal");
    println!("no-send acceptance passed: aggregate native budget before signing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires its own fresh isolated seeded Monad-mode Anvil"]
async fn whole_app_borrower_fills_lender_order_then_owner_withdraws_actual_profit() {
    let mut fixture = Fixture::fresh().await; let key = fixture.key();
    fixture.local.send(fixture.seed.provider_a, fixture.seed.asset,
        TestToken::mintCall { amount: U256::from(100 * WAD) }).await;
    fixture.local.send(fixture.seed.provider_a, fixture.seed.asset,
        TestToken::approveCall { spender: fixture.seed.pool, amount: U256::from(100 * WAD) }).await;
    fixture.local.send(fixture.seed.provider_a, fixture.seed.pool,
        CreditPool::depositCall { recipient: fixture.seed.provider_a, amount: 100 * WAD }).await;
    fixture.local.send(fixture.seed.provider_a, fixture.seed.pool,
        CreditPool::placeProvideOrderCall { account: fixture.seed.provider_a, market: key.market, APR: U256::from(1000), amount: 100 * WAD }).await;
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.collateral,
        TestToken::mintCall { amount: U256::from(400 * WAD) }).await;
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.collateral,
        TestToken::approveCall { spender: fixture.seed.pool, amount: U256::from(400 * WAD) }).await;
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.pool,
        CreditPool::depositCollateralCall { account: fixture.seed.borrower_a, market: key.market, amount: 400 * WAD }).await;
    let filled = fixture.local.send(fixture.seed.borrower_a, fixture.seed.pool,
        CreditPool::borrowCall { account: fixture.seed.borrower_a, market: key.market, amount: 90 * WAD, maxDebt: 100 * WAD }).await;
    assert!(filled["logs"].as_array().unwrap().iter().any(|log|
        log["address"].as_str().unwrap().eq_ignore_ascii_case(&fixture.seed.pool.to_string())
            && log["topics"][0] == json!(CreditPool::Borrowed::SIGNATURE_HASH)));
    fixture.local.send(fixture.seed.borrower_a, fixture.seed.oracle,
        TestOracle::setPriceCall { newPrice: U256::from(WAD * 4 / 10) }).await;
    let snapshot = fixture.snapshot().await; assert!(risk(&snapshot, key).eligible);
    let asset_before = fixture.executor_asset(&snapshot.as_of).await; assert_eq!(asset_before, U256::ZERO);
    let native_before = fixture.operator_native().await;
    fixture.local.rpc("evm_setNextBlockTimestamp", json!([snapshot.as_of.timestamp + 1])).await;
    let miner = advance_finality_after_nonce(&fixture.local, fixture.seed.operator);
    let report = tokio::time::timeout(Duration::from_secs(60), run(fixture.manifest.clone(),
        RunOptions { live: true, cycles: 30 }, Some(fixture.signer()))).await.unwrap().unwrap();
    miner.await.unwrap(); assert_eq!(report.finalized_liquidations, 1); assert!(!report.active_pending);
    let outcome = fixture.assert_success(&report, asset_before, native_before).await;
    let gain = outcome.asset_gain.unwrap(); let after = fixture.reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(fixture.reader.contract_call(fixture.seed.pool, &CreditPool::debtOfCall {
        market: key.market, account: key.borrower }, &after).await.unwrap(), 0);
    let owner_before = fixture.reader.contract_call(fixture.seed.asset,
        &TestToken::balanceOfCall { account: fixture.seed.owner }, &after).await.unwrap();
    // Owner withdrawal is a separate authorized transaction; the operator did not receive the gain.
    assert_eq!(fixture.reader.contract_call(fixture.seed.asset,
        &TestToken::balanceOfCall { account: fixture.seed.operator }, &after).await.unwrap(), U256::ZERO);
    fixture.local.send(fixture.seed.owner, fixture.seed.executor,
        ExecutorOwner::withdrawCall { token: fixture.seed.asset, recipient: fixture.seed.owner, amount: gain }).await;
    let withdrawn = fixture.reader.head(Commitment::Latest).await.unwrap();
    assert_eq!(fixture.executor_asset(&withdrawn).await, asset_before);
    assert_eq!(fixture.reader.contract_call(fixture.seed.asset,
        &TestToken::balanceOfCall { account: fixture.seed.owner }, &withdrawn).await.unwrap(), owner_before + gain);
    let restart = run(fixture.manifest.clone(), RunOptions { live: true, cycles: 3 }, Some(fixture.signer())).await.unwrap();
    assert_eq!(restart.finalized_liquidations, 0); assert!(!restart.active_pending);
    assert_eq!(restart.native_spent,U256::ZERO,"session totals reset on restart");
    assert_eq!(report.last_outcome.as_ref(),Some(&outcome),"withdrawal does not change finalized evidence");
}
