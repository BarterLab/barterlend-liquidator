//! Loopback-only regression tests for bounded, exact-anchor V3 metadata reuse.
use std::{collections::{BTreeMap, BTreeSet}, io::{BufRead, BufReader, Read, Write}, net::{TcpListener, TcpStream}, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}}, thread, time::Duration};

use alloy::{primitives::{Address, B256, Bytes, U256, aliases::{I24, U24, U160}, keccak256}, sol, sol_types::{SolCall, SolValue}};
use barterlend_liquidator::routes::{ExactInputRequest, ExactOutputRequest, RouteKind, RouteLimits, RouteProvider, V3CodeHashes, V3Config, V3RouteProvider, uniswap_v3::compute_pool_address};
use barterlend_state::{BlockRef, ChainIdentity, Commitment, HttpReader, PinnedRead, rpc::{BlockPinning, RpcConfig}};
use serde_json::{Value, json};

sol! {
    interface Reads {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
        function feeAmountTickSpacing(uint24 fee) external view returns (int24);
        function factory() external view returns (address);
        function WETH9() external view returns (address);
        function decimals() external view returns (uint8);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256, uint160[], uint32[], uint256);
        function quoteExactOutput(bytes path, uint256 amountOut) external returns (uint256, uint160[], uint32[], uint256);
    }
}
fn addr(n: u8) -> Address { Address::repeat_byte(n) }
fn block(number: u64) -> BlockRef {
    BlockRef { number, hash: B256::repeat_byte(number as u8), parent_hash: B256::repeat_byte(number.saturating_sub(1) as u8), timestamp: number * 10, commitment: Commitment::Latest }
}
fn chain() -> ChainIdentity { ChainIdentity { chain_id: 31337, genesis_hash: block(0).hash } }
// The real genesis must be nonzero; numbered zero uses a distinct fixture hash.
fn fixture_chain() -> ChainIdentity { ChainIdentity { genesis_hash: B256::repeat_byte(99), ..chain() } }
fn config() -> V3Config {
    let hash = keccak256([0x60, 0x01]);
    V3Config { chain: fixture_chain(), factory: addr(10), router: addr(11), quoter: addr(12), wrapped_native: addr(3),
        code_hashes: V3CodeHashes { factory: hash, router: hash, quoter: hash, wrapped_native: hash },
        allowed_tokens: BTreeSet::from([addr(1), addr(2), addr(3)]), intermediates: vec![addr(3)], fee_tiers: vec![500, 3000],
        pool_code_hashes: BTreeMap::new(), limits: RouteLimits::default() }
}
struct Stub {
    url: String,
    calls: Arc<Mutex<Vec<(u64, String)>>>,
    pins: Arc<Mutex<Vec<Value>>>,
    fork_on_read: Arc<AtomicBool>,
    wrong_chain: Arc<AtomicBool>,
    fork_at: Arc<AtomicU64>,
    wrong_code_at: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Stub {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let pins = Arc::new(Mutex::new(Vec::new()));
        let fork_on_read = Arc::new(AtomicBool::new(false));
        let wrong_chain = Arc::new(AtomicBool::new(false));
        let fork_at = Arc::new(AtomicU64::new(u64::MAX));
        let wrong_code_at = Arc::new(AtomicU64::new(u64::MAX));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, bad_chain, fork, bad_code, stopped) = (calls.clone(), wrong_chain.clone(), fork_at.clone(), wrong_code_at.clone(), stop.clone());
        let (pin_log, fork_on_data) = (pins.clone(), fork_on_read.clone());
        let handle = thread::spawn(move || {
            let mut pools = BTreeMap::new();
            for (a, b) in [(1, 2), (1, 3), (2, 3)] {
                for fee in [500, 3000] {
                    pools.insert(compute_pool_address(addr(10), addr(a), addr(b), fee).unwrap(), (addr(a), addr(b), fee));
                }
            }
            while !stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(1)); continue; }
                    Err(error) => panic!("stub accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" { break; }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") { length = value.trim().parse::<usize>().unwrap(); }
                    }
                }
                if length == 0 { continue; }
                assert!(length <= 1024 * 1024);
                let mut body = vec![0; length]; reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let method = request["method"].as_str().unwrap();
                let parse_number = |value: &Value| u64::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
                let read_number = |pin: &Value| {
                    pin_log.lock().unwrap().push(pin.clone());
                    let number = if let Some(hash) = pin.get("blockHash") {
                        assert_eq!(pin["requireCanonical"], true);
                        let hash: B256 = serde_json::from_value(hash.clone()).unwrap(); u64::from(hash[0])
                    } else if pin.as_str().is_some_and(|value| value.len() == 66) {
                        let hash: B256 = serde_json::from_value(pin.clone()).unwrap(); u64::from(hash[0])
                    } else { parse_number(pin) };
                    if fork_on_data.swap(false, Ordering::AcqRel) { fork.store(number, Ordering::Release); }
                    number
                };
                let result = match method {
                    "eth_chainId" => json!(if bad_chain.load(Ordering::Acquire) { "0x1" } else { "0x7a69" }),
                    "eth_getBlockByNumber" => {
                        let number = parse_number(&request["params"][0]); let mut at = block(number);
                        if number == 0 { at.hash = fixture_chain().genesis_hash; }
                        if fork.load(Ordering::Acquire) == number { at.hash = B256::repeat_byte(254); }
                        json!({ "number": format!("0x{:x}", number), "hash": at.hash, "parentHash": at.parent_hash, "timestamp": format!("0x{:x}", at.timestamp) })
                    }
                    "eth_getCode" => {
                        let number = read_number(&request["params"][1]);
                        log.lock().unwrap().push((number, "code".to_string()));
                        json!(if bad_code.load(Ordering::Acquire) == number { "0xdead" } else { "0x6001" })
                    }
                    "eth_call" => {
                        let number = read_number(&request["params"][1]);
                        let to: Address = serde_json::from_value(request["params"][0]["to"].clone()).unwrap();
                        let data: Bytes = serde_json::from_value(request["params"][0]["data"].clone()).unwrap();
                        let selector = &data[..4];
                        let (name, value) = if selector == Reads::getPoolCall::SELECTOR {
                            let call = Reads::getPoolCall::abi_decode_validate(&data).unwrap();
                            let absent = call.fee == U24::from(500) && [call.tokenA, call.tokenB].contains(&addr(3));
                            ("getPool", if absent { Address::ZERO } else { compute_pool_address(addr(10), call.tokenA, call.tokenB, call.fee.to::<u32>()).unwrap() }.abi_encode())
                        } else if selector == Reads::feeAmountTickSpacingCall::SELECTOR { ("feeSpacing", I24::from_raw(U24::from(10)).abi_encode())
                        } else if selector == Reads::factoryCall::SELECTOR { ("factory", addr(10).abi_encode())
                        } else if selector == Reads::WETH9Call::SELECTOR { ("wrapped", addr(3).abi_encode())
                        } else if selector == Reads::decimalsCall::SELECTOR { ("decimals", U256::from(18).abi_encode())
                        } else if selector == Reads::token0Call::SELECTOR { ("token0", pools[&to].0.abi_encode())
                        } else if selector == Reads::token1Call::SELECTOR { ("token1", pools[&to].1.abi_encode())
                        } else if selector == Reads::feeCall::SELECTOR { ("fee", U24::from(pools[&to].2).abi_encode())
                        } else if selector == Reads::tickSpacingCall::SELECTOR { ("tickSpacing", I24::from_raw(U24::from(10)).abi_encode())
                        } else if selector == Reads::quoteExactInputCall::SELECTOR {
                            let call = Reads::quoteExactInputCall::abi_decode_validate(&data).unwrap();
                            let hops = (call.path.len() - 20) / 23;
                            ("quoteInput", (call.amountIn * U256::from(2) + U256::from(number), vec![U160::from(1); hops], vec![0u32; hops], U256::from(100_000)).abi_encode_params())
                        } else if selector == Reads::quoteExactOutputCall::SELECTOR {
                            let call = Reads::quoteExactOutputCall::abi_decode_validate(&data).unwrap();
                            let hops = (call.path.len() - 20) / 23;
                            ("quoteOutput", (call.amountOut * U256::from(2) + U256::from(number), vec![U160::from(1); hops], vec![0u32; hops], U256::from(100_000)).abi_encode_params())
                        } else { panic!("unknown selector {selector:?}") };
                        log.lock().unwrap().push((number, name.to_string()));
                        json!(Bytes::from(value))
                    }
                    other => panic!("unexpected method {other}"),
                };
                let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": request["id"], "result": result })).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self { url, calls, pins, fork_on_read, wrong_chain, fork_at, wrong_code_at, stop, handle: Some(handle) }
    }
    fn count(&self, number: u64, name: &str) -> usize { self.calls.lock().unwrap().iter().filter(|(n, kind)| *n == number && kind == name).count() }
    fn provider(&self) -> V3RouteProvider {
        let reader = Arc::new(HttpReader::new(&self.url, RpcConfig { min_request_interval: Duration::ZERO, max_retries: 0, ..Default::default() }).unwrap());
        V3RouteProvider::new(reader, config()).unwrap()
    }
}
impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(handle) = self.handle.take() { let result = handle.join(); if !thread::panicking() { result.unwrap(); } }
    }
}
fn input(at: BlockRef) -> ExactInputRequest { ExactInputRequest { token_in: addr(1), token_out: addr(2), amount_in: U256::from(100), executor: addr(20), at } }

#[tokio::test]
async fn shared_and_absent_hops_are_read_once_per_anchor_but_prices_are_not_cached() {
    let stub = Stub::new(); let provider = stub.provider(); let request = input(block(5));
    let quotes = provider.quote_exact_input_candidates(&request).await.unwrap();
    assert_eq!(quotes.len(), 3, "two direct paths and one two-hop path");
    assert_eq!(stub.count(5, "getPool"), 6, "six unique hops, including absent fee-500 hops, regardless of candidate overlap");
    assert_eq!(stub.count(5, "feeSpacing"), 2, "once per configured fee, reused for all pool checks");
    let metadata = stub.count(5, "code"); let prices = stub.count(5, "quoteInput");
    let mut resized = request.clone(); resized.amount_in += U256::from(1);
    let refreshed = provider.quote_exact_input_route(&resized, &quotes[0].route).await.unwrap();
    assert_eq!(refreshed.amount_out, U256::from(207));
    assert_eq!(stub.count(5, "code"), metadata);
    assert_eq!(stub.count(5, "getPool"), 6);
    assert_eq!(stub.count(5, "quoteInput"), prices + 1, "selected path must get a real new price");
    provider.quote_exact_input_candidates(&request).await.unwrap();
    assert_eq!(stub.count(5, "getPool"), 6, "cached absence also survives the next search at the same anchor");
    let mut fresh = resized; fresh.at = block(6);
    let requoted = provider.quote_exact_input_route(&fresh, &quotes[0].route).await.unwrap();
    assert_eq!(requoted.validity.as_of, block(6));
    assert_eq!(requoted.amount_out, U256::from(208));
    assert_ne!(requoted.fingerprint, refreshed.fingerprint);
    assert!(stub.count(6, "code") >= 5, "new anchor checks contracts and selected pools again");
    assert_eq!(stub.count(6, "quoteInput"), 1, "fresh execution must not repeat discovery of unused paths");
}

#[tokio::test]
async fn exact_output_selected_route_reprices_amount_and_preserves_reverse_path() {
    let stub = Stub::new(); let provider = stub.provider();
    let mut request = ExactOutputRequest { token_in: addr(1), token_out: addr(3), amount_out: U256::from(100), executor: addr(20), at: block(5) };
    let selected = provider.quote_exact_output(&request).await.unwrap();
    let metadata = stub.count(5, "code"); let prices = stub.count(5, "quoteOutput");
    request.amount_out = U256::from(101);
    let quote = provider.quote_exact_output_route(&request, &selected.route).await.unwrap();
    quote.validate(request.token_in, request.token_out, request.amount_out, request.executor, &request.at).unwrap();
    assert_eq!(quote.amount_in, U256::from(207));
    assert_eq!(quote.reverse_path, selected.reverse_path);
    assert_eq!(stub.count(5, "code"), metadata);
    assert_eq!(stub.count(5, "quoteOutput"), prices + 1);
}

#[tokio::test]
async fn cached_metadata_never_bypasses_identity_canonicality_or_new_block_code_checks() {
    let stub = Stub::new(); let provider = stub.provider(); let request = input(block(5));
    let selected = provider.quote_exact_input(&request).await.unwrap();
    stub.wrong_chain.store(true, Ordering::Release);
    assert!(provider.quote_exact_input_route(&request, &selected.route).await.is_err());
    stub.wrong_chain.store(false, Ordering::Release);
    stub.fork_at.store(5, Ordering::Release);
    assert!(provider.quote_exact_input_route(&request, &selected.route).await.is_err());
    stub.fork_at.store(u64::MAX, Ordering::Release);
    stub.wrong_code_at.store(6, Ordering::Release);
    let mut newer = request; newer.at = block(6);
    assert!(provider.quote_exact_input_route(&newer, &selected.route).await.is_err());
    assert_eq!(stub.count(6, "quoteInput"), 0, "failed fresh identity check must prevent a price quote");
}

#[tokio::test]
async fn selected_route_cannot_inject_a_pool_or_escape_configured_intermediates() {
    let stub = Stub::new(); let provider = stub.provider(); let request = input(block(5));
    let selected = provider.quote_exact_input(&request).await.unwrap();
    let mut route = selected.route;
    if let RouteKind::V3 { pools, .. } = &mut route { pools[0] = addr(250); }
    assert!(provider.quote_exact_input_route(&request, &route).await.is_err());
    assert!(provider.quote_exact_input_route(&request, &RouteKind::SameToken).await.is_err());
    let mut config = config(); config.intermediates.clear();
    let no_middle = V3RouteProvider::new(provider.reader().clone(), config).unwrap();
    let tokens = vec![addr(1), addr(3), addr(2)]; let fees = vec![3000, 3000];
    let pools = vec![compute_pool_address(addr(10), tokens[0], tokens[1], fees[0]).unwrap(), compute_pool_address(addr(10), tokens[1], tokens[2], fees[1]).unwrap()];
    let route = RouteKind::V3 { path: barterlend_liquidator::routes::encode_exact_input_path(&tokens, &fees).unwrap(), tokens, fees, pools };
    assert!(no_middle.quote_exact_input_route(&request, &route).await.is_err());
}

#[tokio::test]
async fn bundles_preserve_pinning_order_limits_and_reject_a_reorg_during_reads() {
    for pinning in [BlockPinning::NumberBracket, BlockPinning::Eip1898, BlockPinning::RawHash] {
        let stub = Stub::new();
        let reader = HttpReader::new(&stub.url, RpcConfig { pinning, min_request_interval: Duration::ZERO, max_retries: 0, ..Default::default() }).unwrap();
        let reads = [PinnedRead::Code { address: addr(10) }, PinnedRead::call(addr(11), &Reads::factoryCall {})];
        let values = reader.read_bundle(&reads, &block(5)).await.unwrap();
        assert_eq!(values, vec![Bytes::from(vec![0x60, 0x01]), Bytes::from(addr(10).abi_encode())], "bundle returns input order");
        let expected = match pinning {
            BlockPinning::NumberBracket => json!("0x5"),
            BlockPinning::Eip1898 => json!({ "blockHash": block(5).hash, "requireCanonical": true }),
            BlockPinning::RawHash => json!(block(5).hash),
        };
        assert_eq!(*stub.pins.lock().unwrap(), vec![expected.clone(), expected]);
        assert!(reader.read_bundle(&vec![reads[0].clone(); 65], &block(5)).await.is_err());
        assert_eq!(stub.pins.lock().unwrap().len(), 2, "oversized bundle is rejected before RPC");
        stub.fork_on_read.store(true, Ordering::Release);
        assert!(matches!(reader.read_bundle(&reads, &block(5)).await, Err(barterlend_state::Error::NonCanonical(5))), "post-read header must invalidate all values on a reorg");
    }
}

#[tokio::test]
async fn bounded_search_rotates_all_paths_instead_of_starving_the_tail() {
    let stub = Stub::new();
    let reader = Arc::new(HttpReader::new(&stub.url, RpcConfig { min_request_interval: Duration::ZERO, max_retries: 0, ..Default::default() }).unwrap());
    let mut cfg = config(); cfg.limits.max_candidates=1; cfg.limits.max_quote_calls=1;
    let provider = V3RouteProvider::new(reader,cfg).unwrap();
    let mut seen=std::collections::BTreeSet::new();
    let mut unavailable=0;
    for _ in 0..6 {
        match provider.quote_exact_input(&input(block(5))).await {
            Ok(quote)=>{assert_eq!(quote.search.quote_calls,1);assert!(quote.search.budget_exhausted);assert!(seen.insert(quote.route.exact_input_path()));},
            Err(barterlend_liquidator::routes::RouteError::Unavailable(_))=>unavailable+=1,
            Err(error)=>panic!("unexpected error: {error}"),
        }
    }
    assert_eq!(unavailable,3,"fixture has three paths with absent pools");
    assert_eq!(seen.len(),3);
    assert!(seen.iter().any(|path|path.len()==66),"last two-hop path must be reached after unavailable paths");
    assert!(seen.contains(&provider.quote_exact_input(&input(block(5))).await.unwrap().route.exact_input_path()));
}
