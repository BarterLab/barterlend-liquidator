// Whole-app acceptance against genuine isolated Anvil, with a transport-only local fault proxy.
// Healthy replies always come from the origin node; no chain state or receipt is fabricated.

use std::{collections::{BTreeMap, BTreeSet}, io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream}, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread, time::{Duration, Instant}};

use alloy::{primitives::{Address, B256, Bytes, U256, keccak256},
    sol, sol_types::SolCall};
use barterlend_liquidator::{app::RunReport, economics::FeePolicy,
    local_config::{LocalManifest, LocalPoolManifest}, planner::PlannerConfig,
    routes::{RouteLimits, V3CodeHashes, V3Config},
    tx::{OutcomeKind, rpc::{quantity_u64, validate_loopback_url}}};
use barterlend_state::{*, abi::CreditPool, rpc::RpcConfig};
use serde::Deserialize;
use serde_json::{Value, json};

const WAD: u128 = 1_000_000_000_000_000_000;
// Public test account, used only on the explicit loopback fixture node.
const LOCAL_DEV_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
include!("sync_policy.rs");

sol! {
    interface TestToken {
        function mint(uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
    }
    interface TestOracle { function setPrice(uint256 newPrice) external; }
}

#[derive(Deserialize)]
struct Seed {
    chain_id: u64, genesis_hash: B256, pool: Address, asset: Address, collateral: Address,
    oracle: Address, factory: Address, router: Address, quoter: Address, wrapped_native: Address,
    executor: Address, owner: Address, operator: Address, provider_a: Address,
    borrower_a: Address, borrower_b: Address, market0: String, runtime_code_hashes: BTreeMap<String, B256>,
}

struct LocalTx { client: reqwest::Client, url: reqwest::Url, timestamp: u64 }

impl LocalTx {
    async fn rpc(&self, method: &str, params: Value) -> Value {
        assert!(matches!(method, "eth_chainId" | "eth_blockNumber" | "eth_getBlockByNumber"
            | "eth_getTransactionReceipt" | "anvil_impersonateAccount" | "anvil_mine"
            | "anvil_setCode" | "eth_sendTransaction" | "eth_getBalance" | "eth_getTransactionCount"
            | "evm_setNextBlockTimestamp" | "evm_mine" | "evm_snapshot" | "evm_revert"));
        let response: Value = self.client.post(self.url.clone())
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        assert!(response.get("error").is_none(), "origin {method}: {response}");
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
            if !receipt.is_null() { assert_eq!(receipt["status"], "0x1", "setup reverted: {receipt}"); return receipt; }
            assert!(Instant::now() < deadline, "setup did not mine");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[derive(Default)]
struct ProxyStats {
    outage: bool, block_raw: bool, hold_identity: bool, held_identity: usize, forwarding: usize, rejected: usize, blocked_raw: usize,
    raw_requests: Vec<Bytes>, forwarded_raw: Vec<Bytes>,
}

struct FaultProxy {
    url: String, stats: Arc<Mutex<ProxyStats>>, stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FaultProxy {
    fn new(origin: reqwest::Url) -> Self {
        validate_loopback_url(origin.as_str()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stats = Arc::new(Mutex::new(ProxyStats::default()));
        let counters = stats.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let runtime = tokio::runtime::Handle::current();
        let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let thread = thread::spawn(move || {
            let mut connections = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1)); continue;
                    }
                    Err(error) => panic!("proxy listener: {error}"),
                };
                if stopping.load(Ordering::Acquire) { break; }
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let stats = counters.clone(); let client = client.clone();
                let origin = origin.clone(); let runtime = runtime.clone(); let stopping = stopping.clone();
                connections.push(thread::spawn(move || {
                    let Some(request) = read_request(&stream) else { return; };
                    let hold_identity = {
                        let mut data = stats.lock().unwrap();
                        let held = data.hold_identity && request["method"] == "eth_chainId";
                        if held { data.held_identity += 1; }
                        held
                    };
                    if hold_identity {
                        while !stopping.load(Ordering::Acquire) && stats.lock().unwrap().hold_identity {
                            thread::sleep(Duration::from_millis(5));
                        }
                        return; // No response/forward before the interrupted child has exited.
                    }
                    let outage = {
                        let mut data = stats.lock().unwrap();
                        let raw = if request["method"] == "eth_sendRawTransaction" {
                            let raw: Bytes = serde_json::from_value(request["params"][0].clone()).unwrap();
                            data.raw_requests.push(raw.clone()); Some(raw)
                        } else { None };
                        if data.outage { data.rejected += 1; true }
                        else if raw.is_some() && data.block_raw { data.blocked_raw += 1; true }
                        else {
                            if let Some(raw) = raw { data.forwarded_raw.push(raw); }
                            data.forwarding += 1; false
                        }
                    };
                    let response = if outage {
                        json!({"jsonrpc":"2.0","id":request["id"],
                            "error":{"code":-32000,"message":"forced isolated RPC outage"}})
                    } else {
                        let result = runtime.block_on(async {
                            client.post(origin).json(&request).send().await?
                                .error_for_status()?.json::<Value>().await
                        });
                        stats.lock().unwrap().forwarding -= 1;
                        result.expect("healthy proxy must forward actual origin response")
                    };
                    let body = serde_json::to_vec(&response).unwrap();
                    if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).is_err() { return; }
                    let _ = stream.write_all(&body);
                }));
            }
            for connection in connections { connection.join().unwrap(); }
        });
        Self { url, stats, stop, thread: Some(thread) }
    }

    fn raw_requests(&self) -> Vec<Bytes> { self.stats.lock().unwrap().raw_requests.clone() }
    fn forwarded_raw(&self) -> Vec<Bytes> { self.stats.lock().unwrap().forwarded_raw.clone() }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.url.strip_prefix("http://").unwrap());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() && !thread::panicking() { panic!("fault proxy worker failed"); }
        }
    }
}

fn read_request(stream: &TcpStream) -> Option<Value> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 || !line.starts_with("POST ") { return None; }
    let mut length = None;
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 { return None; }
        if line == "\r\n" { break; }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().ok()?);
        }
    }
    let length = length?;
    assert!(length <= 1024 * 1024, "unexpected oversized proxy request");
    let mut body = vec![0; length]; reader.read_exact(&mut body).ok()?;
    Some(serde_json::from_slice(&body).expect("app must send valid JSON-RPC"))
}

async fn wait_for<T>(description: &str, timeout: Duration, mut observe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = observe() { return value; }
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// The shared fixture is also used by subprocess-only CLI acceptance.
#[allow(dead_code)]
struct AppTask(Option<tokio::task::JoinHandle<color_eyre::eyre::Result<RunReport>>>);
#[allow(dead_code)]
impl AppTask {
    async fn finish(&mut self) -> RunReport {
        let task = self.0.as_mut().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(45), task).await
            .expect("bounded application did not stop").expect("app task panicked").expect("app run failed");
        self.0.take(); result
    }
}
impl Drop for AppTask { fn drop(&mut self) { if let Some(task) = self.0.take() { task.abort(); } } }

fn manifest(seed: &Seed, url: String, chain: ChainIdentity, oracle_code_hash: B256, _runtime_dir: &Path) -> LocalManifest {
    LocalManifest { discovery: None, v4: None, kyber: None, native_price_feeds: Vec::new(), rpc: barterlend_liquidator::local_config::RpcPolicy { requests_per_second: 1000, ..Default::default() }, sync: local_sync_policy(), rpc_fallback_urls: Vec::new(), recovery: Default::default(), cancellation_retry_seconds: 30, sdk_history_chunks_per_step: 8, health_listen: None, schema: 1, profile: "local".into(), rpc_url: url, chain,
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

async fn open_fault_position(local:&mut LocalTx, seed:&Seed, pool:Address, borrower:Address, market:u64, collateral:u128) {
    local.send(borrower,seed.collateral,TestToken::mintCall{amount:U256::from(collateral)}).await;
    local.send(borrower,seed.collateral,TestToken::approveCall{spender:pool,amount:U256::from(collateral)}).await;
    local.send(borrower,pool,CreditPool::depositCollateralCall{account:borrower,market,amount:collateral}).await;
    local.send(borrower,pool,CreditPool::placeBorrowOrderCall{account:borrower,market,APR:U256::from(1000),amount:100*WAD}).await;
    local.send(seed.provider_a,seed.asset,TestToken::mintCall{amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.asset,TestToken::approveCall{spender:pool,amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,pool,CreditPool::depositCall{recipient:seed.provider_a,amount:100*WAD}).await;
    local.send(seed.provider_a,pool,CreditPool::provideExactInCall{account:seed.provider_a,market,assetsAmount:100*WAD,minLoanAmount:0,maxCrossedOrders:10}).await;
}
