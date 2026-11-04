//! Transport policy tests use only a loopback HTTP stub; no public acceptance is implied.
use std::{io::{BufRead, BufReader, Read, Write}, net::{TcpListener, TcpStream},
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, thread, time::{Duration, SystemTime, UNIX_EPOCH}};
use alloy::{primitives::{Address, B256, Bytes, U256, keccak256}, signers::local::PrivateKeySigner};
use alloy::sol_types::SolCall;
use barterlend_liquidator::{executor_abi::{LiquidationExecutor, Request}, recovery::{EndpointHealth, RecoveryPolicy},
    tx::{AttemptKind, PlanMeta, SignedAttempt, UnsignedTx, rpc::{LocalTxRpc, TxRpc}}};
use barterlend_state::{BlockRef, ChainIdentity, Commitment, HttpReader, PositionKey, rpc::RpcConfig};
use serde_json::{Value, json};

struct IdentityStub {
    url: String, chain: Arc<Mutex<ChainIdentity>>, methods: Arc<Mutex<Vec<String>>>,
    head: Arc<Mutex<BlockRef>>, switch_on_latest: Arc<Mutex<Option<Arc<HttpReader>>>>,
    stop: Arc<AtomicBool>, thread: Option<thread::JoinHandle<()>>,
}

impl IdentityStub {
    fn new(identity: ChainIdentity) -> Self {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let chain=Arc::new(Mutex::new(identity));let server_chain=chain.clone();
        let head=Arc::new(Mutex::new(test_head(1,unix_seconds())));let server_head=head.clone();
        let switch_on_latest=Arc::new(Mutex::new(None::<Arc<HttpReader>>));let server_switch=switch_on_latest.clone();
        let methods=Arc::new(Mutex::new(Vec::new()));let server_methods=methods.clone();
        let stop=Arc::new(AtomicBool::new(false));let server_stop=stop.clone();
        let thread=thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                let (mut stream,_) = match listener.accept() {
                    Ok(value)=>value,
                    Err(error) if error.kind()==std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(1));continue; },
                    Err(error)=>panic!("stub accept: {error}"),
                };
                // macOS can inherit O_NONBLOCK from the listening socket. HTTP reads
                // must wait for the complete request rather than racing its arrival.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut reader=BufReader::new(stream.try_clone().unwrap());let mut length=0;
                loop {
                    let mut line=String::new();if reader.read_line(&mut line).unwrap()==0 { break; }
                    if line=="\r\n" { break; }
                    if let Some((name,value))=line.split_once(':') { if name.eq_ignore_ascii_case("content-length") { length=value.trim().parse::<usize>().unwrap(); } }
                }
                if length==0 { continue; }
                assert!(length<=1024*1024);let mut bytes=vec![0;length];reader.read_exact(&mut bytes).unwrap();
                let request:Value=serde_json::from_slice(&bytes).unwrap();let method=request["method"].as_str().unwrap();
                server_methods.lock().unwrap().push(method.to_string());
                let identity=*server_chain.lock().unwrap();
                let result=match method {
                    "eth_chainId"=>json!(format!("0x{:x}",identity.chain_id)),
                    "eth_getBlockByNumber"=>{
                        if request["params"][0] == "0x0" { json!({"number":"0x0","hash":identity.genesis_hash,"parentHash":B256::ZERO,"timestamp":"0x0"}) }
                        else {
                            if let Some(reader)=server_switch.lock().unwrap().take() { reader.rpc().select_endpoint(1).unwrap(); }
                            let head=*server_head.lock().unwrap();
                            json!({"number":format!("0x{:x}",head.number),"hash":head.hash,"parentHash":head.parent_hash,"timestamp":format!("0x{:x}",head.timestamp)})
                        }
                    },
                    "eth_sendRawTransaction"=>{let raw:Bytes=serde_json::from_value(request["params"][0].clone()).unwrap();json!(keccak256(raw))},
                    other=>panic!("unexpected stub method {other}"),
                };
                let body=serde_json::to_vec(&json!({"jsonrpc":"2.0","id":request["id"],"result":result})).unwrap();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self { url,chain,methods,head,switch_on_latest,stop,thread:Some(thread) }
    }
}

fn unix_seconds() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() }
fn test_head(number:u64,timestamp:u64) -> BlockRef {
    BlockRef { number,timestamp,hash:B256::repeat_byte((number+1) as u8),parent_hash:B256::repeat_byte(number as u8),commitment:Commitment::Latest }
}
async fn cancel_raw() -> Bytes {
    let signer=PrivateKeySigner::from_slice(&[1u8;32]).unwrap();
    UnsignedTx { chain_id:10143,nonce:0,to:signer.address(),input:Bytes::new(),gas_limit:21_000,gas_price:U256::from(1) }.sign(&signer).await.unwrap()
}
fn transport(stub:&IdentityStub,identity:ChainIdentity) -> LocalTxRpc {
    let reader=Arc::new(HttpReader::new(&stub.url,RpcConfig { min_request_interval:Duration::ZERO,..Default::default() }).unwrap());
    LocalTxRpc::for_profile(&stub.url,reader,"monad-testnet-proxy",identity).unwrap()
}
fn send_count(stub:&IdentityStub) -> usize {
    stub.methods.lock().unwrap().iter().filter(|method|method.as_str()=="eth_sendRawTransaction").count()
}

impl Drop for IdentityStub {
    fn drop(&mut self) {
        self.stop.store(true,Ordering::Release);
        let _=TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(handle)=self.thread.take() {
            let result=handle.join();
            // Preserve the original assertion failure instead of aborting on a
            // second panic during unwinding. Otherwise the stub failure is fatal.
            if !thread::panicking() { result.expect("identity RPC stub thread failed"); }
        }
    }
}

#[tokio::test]
async fn testnet_transport_rechecks_chain_and_genesis_on_every_raw_send() {
    let identity=ChainIdentity { chain_id:10143,genesis_hash:B256::repeat_byte(11) };
    let stub=IdentityStub::new(identity);
    let reader=Arc::new(HttpReader::new(&stub.url,RpcConfig { min_request_interval:Duration::ZERO,..Default::default() }).unwrap());
    let rpc=LocalTxRpc::for_profile(&stub.url,reader,"monad-testnet-proxy",identity).unwrap();
    let signer=PrivateKeySigner::from_slice(&[1u8;32]).unwrap();
    let mut unsigned=UnsignedTx { chain_id:143,nonce:0,to:Address::repeat_byte(3),input:Bytes::new(),gas_limit:21_000,gas_price:U256::from(1) };
    assert!(rpc.send_raw(&unsigned.sign(&signer).await.unwrap()).await.is_err());
    assert!(stub.methods.lock().unwrap().is_empty(),"mainnet signature must be rejected before RPC");
    unsigned.chain_id=10143;let raw=unsigned.sign(&signer).await.unwrap();
    let mut noncanonical=raw.to_vec();noncanonical.push(0);
    assert!(rpc.send_raw(&Bytes::from(noncanonical)).await.is_err());
    assert!(stub.methods.lock().unwrap().is_empty());
    assert_eq!(rpc.send_raw(&raw).await.unwrap(),keccak256(&raw));
    stub.chain.lock().unwrap().genesis_hash=B256::repeat_byte(12);
    assert!(rpc.send_raw(&raw).await.is_err());
    *stub.chain.lock().unwrap()=ChainIdentity {chain_id:143,genesis_hash:identity.genesis_hash};
    assert!(rpc.send_raw(&raw).await.is_err());
    let methods=stub.methods.lock().unwrap().clone();
    assert_eq!(methods.iter().filter(|method|method.as_str()=="eth_sendRawTransaction").count(),1);
    assert_eq!(methods.iter().filter(|method|method.as_str()=="eth_chainId").count(),3);
}

#[tokio::test]
async fn successful_stale_head_responses_cannot_send_and_fresh_head_recovers_exact_signed_bytes() {
    let identity=ChainIdentity {chain_id:10143,genesis_hash:B256::repeat_byte(11)};
    let stub=IdentityStub::new(identity);let rpc=transport(&stub,identity);let raw=cancel_raw().await;
    *stub.head.lock().unwrap()=test_head(1,unix_seconds()-3600);
    let error=rpc.send_raw(&raw).await.unwrap_err();
    assert!(error.to_string().contains("wall-clock age budget"));
    assert_eq!(send_count(&stub),0,"HTTP200 and matching chain identity cannot authorize a stale source");
    *stub.head.lock().unwrap()=test_head(2,unix_seconds());
    assert_eq!(rpc.send_raw(&raw).await.unwrap(),keccak256(&raw));
    *stub.head.lock().unwrap()=test_head(3,unix_seconds()+3600);
    assert!(rpc.send_raw(&raw).await.unwrap_err().to_string().contains("local clock tolerance"));
    assert_eq!(send_count(&stub),1);
}

#[tokio::test]
async fn shared_frozen_head_progress_guard_blocks_resend_until_a_higher_head_arrives() {
    let identity=ChainIdentity {chain_id:10143,genesis_hash:B256::repeat_byte(11)};
    let stub=IdentityStub::new(identity);let mut rpc=transport(&stub,identity);let raw=cancel_raw().await;
    let policy=RecoveryPolicy { max_no_progress_seconds:1,..Default::default() };
    let health=Arc::new(EndpointHealth::new(policy,true,1));
    health.observe(0,*stub.head.lock().unwrap(),unix_seconds()).unwrap();
    rpc.set_health_monitor(health);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(rpc.send_raw(&raw).await.unwrap_err().to_string().contains("stopped advancing"));
    assert_eq!(send_count(&stub),0);
    *stub.head.lock().unwrap()=test_head(2,unix_seconds());
    assert_eq!(rpc.send_raw(&raw).await.unwrap(),keccak256(&raw));
    assert_eq!(send_count(&stub),1);
}

async fn liquidation_signed(as_of:BlockRef,deadline:u64,max_valid_block:u64) -> SignedAttempt {
    let signer=PrivateKeySigner::from_slice(&[1u8;32]).unwrap();
    let key=PositionKey {pool:Address::repeat_byte(2),market:1000,borrower:Address::repeat_byte(3)};
    let request=Request {pool:key.pool,market:key.market,borrower:key.borrower,requestedDebt:100,
        minDiscountOut:U256::from(1),deadline:U256::from(deadline),path:Bytes::new(),minAssetProfit:U256::from(1), minSwapOutput: U256::from(1),planFingerprint:B256::repeat_byte(9)};
    let envelope=UnsignedTx {chain_id:10143,nonce:0,to:Address::repeat_byte(4),
        input:LiquidationExecutor::executeCall {request}.abi_encode().into(),gas_limit:100_000,gas_price:U256::from(1)};
    let meta=PlanMeta {key,epoch:1,as_of,deadline,max_valid_block,requested_debt:U256::from(100),min_discount:U256::from(1),min_asset_profit:U256::from(1), min_swap_output: Some(U256::from(1)),
        plan_fingerprint:B256::repeat_byte(9),simulation_fingerprint:envelope.fingerprint().unwrap(),paid_state_fingerprint:B256::repeat_byte(10),asset:Address::repeat_byte(5)};
    let raw=envelope.sign(&signer).await.unwrap();
    SignedAttempt {kind:AttemptKind::Liquidation(meta),envelope,hash:keccak256(&raw),raw,created_at:as_of.timestamp,broadcast_at:None,created_at_unix_ms:None}
}

#[tokio::test]
async fn actual_send_rechecks_liquidation_time_and_block_bounds_after_identity_reads() {
    let identity=ChainIdentity {chain_id:10143,genesis_hash:B256::repeat_byte(11)};
    let stub=IdentityStub::new(identity);let rpc=transport(&stub,identity);
    let now=unix_seconds();let anchor=test_head(1,now-2);
    *stub.head.lock().unwrap()=test_head(2,now);
    let time_expired=liquidation_signed(anchor,now-1,100).await;
    let block_expired=liquidation_signed(anchor,now+3600,1).await;
    for attempt in [time_expired,block_expired] {
        assert!(rpc.send_attempt(&attempt).await.unwrap_err().to_string().contains("bounds expired or changed"));
        assert_eq!(send_count(&stub),0);
    }
    let valid=liquidation_signed(anchor,now+3600,100).await;
    assert_eq!(rpc.send_attempt(&valid).await.unwrap(),valid.hash);
    assert_eq!(send_count(&stub),1);
}

#[tokio::test]
async fn endpoint_switch_during_final_head_read_cannot_redirect_signed_bytes() {
    let identity=ChainIdentity {chain_id:10143,genesis_hash:B256::repeat_byte(11)};
    let first=IdentityStub::new(identity);let second=IdentityStub::new(identity);
    let urls=vec![first.url.clone(),second.url.clone()];
    let reader=Arc::new(HttpReader::with_endpoints(&urls,RpcConfig {min_request_interval:Duration::ZERO,..Default::default()}).unwrap());
    let rpc=LocalTxRpc::for_endpoints(&urls,reader.clone(),"monad-testnet-proxy",identity).unwrap();
    *first.switch_on_latest.lock().unwrap()=Some(reader);
    let raw=cancel_raw().await;
    assert!(rpc.send_raw(&raw).await.unwrap_err().to_string().contains("endpoint changed during send validation"));
    assert_eq!(send_count(&first),0);assert_eq!(send_count(&second),0);
}

#[test]
fn testnet_transport_preserves_endpoint_binding_and_requires_pinned_identity() {
    let identity=ChainIdentity {chain_id:10143,genesis_hash:B256::repeat_byte(11)};
    let url="https://testnet-rpc.monad.xyz";
    let reader=Arc::new(HttpReader::new(url,RpcConfig::default()).unwrap());
    assert!(LocalTxRpc::new(url,reader.clone()).is_err());
    assert!(LocalTxRpc::for_profile(url,reader.clone(),"monad-testnet",identity).is_ok());
    assert!(LocalTxRpc::for_profile("https://testnet-rpc.monad.xyz/other",reader.clone(),"monad-testnet",identity).is_err());
    assert!(LocalTxRpc::for_profile(url,reader,"monad-testnet",ChainIdentity {genesis_hash:B256::ZERO,..identity}).is_err());
}
