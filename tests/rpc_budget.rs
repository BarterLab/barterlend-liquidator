//! Shared SDK/transaction admission tests. Only an in-process loopback stub is used.
use std::{io::{BufRead,BufReader,Read,Write}, net::{TcpListener,TcpStream},
    sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}}, thread,
    time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use alloy::{primitives::{Address,B256,Bytes,U256,keccak256},signers::local::PrivateKeySigner};
use barterlend_liquidator::{local_config::RpcPolicy,tx::{UnsignedTx,rpc::{LocalTxRpc,TxRpc}}};
use barterlend_state::{BlockRef,ChainIdentity,Commitment,HttpReader};
use serde_json::{Value,json};

struct Stub {
    url:String, sends:Arc<AtomicUsize>,
    reject_sends:Arc<AtomicBool>, retry_reads:Arc<AtomicUsize>, stop:Arc<AtomicBool>,
    worker:Option<thread::JoinHandle<()>>, head:BlockRef,
}
impl Stub {
    fn new(arrivals:Arc<Mutex<Vec<(Instant,String)>>>) -> Self {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());let stop=Arc::new(AtomicBool::new(false));
        let head=BlockRef { number:1,hash:B256::repeat_byte(2),parent_hash:B256::repeat_byte(1),
            timestamp:SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),commitment:Commitment::Latest };
        let sends=Arc::new(AtomicUsize::new(0));let reject_sends=Arc::new(AtomicBool::new(false));let retry_reads=Arc::new(AtomicUsize::new(0));
        let (stopping,seen,sent,reject,retries)=(stop.clone(),arrivals.clone(),sends.clone(),reject_sends.clone(),retry_reads.clone());
        let worker=thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream,_) = match listener.accept() {
                    Ok(pair)=>pair,Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>{thread::sleep(Duration::from_millis(1));continue;},Err(error)=>panic!("stub accept: {error}")
                };
                if stopping.load(Ordering::Acquire) { break; }
                stream.set_nonblocking(false).unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut input=BufReader::new(stream.try_clone().unwrap());let mut length=0;
                loop { let mut line=String::new();if input.read_line(&mut line).unwrap_or(0)==0 || line=="\r\n" {break;}
                    if let Some((name,value))=line.split_once(':') { if name.eq_ignore_ascii_case("content-length") {length=value.trim().parse::<usize>().unwrap();} }
                }
                if length==0 { continue; } assert!(length<1024*1024);let mut body=vec![0;length];input.read_exact(&mut body).unwrap();
                let request:Value=serde_json::from_slice(&body).unwrap();let method=request["method"].as_str().unwrap();seen.lock().unwrap().push((Instant::now(),method.to_owned()));
                let result=match method {
                    "eth_chainId"=>json!("0x7a69"),
                    "eth_getBlockByNumber"=>if request["params"][0]=="0x0" {json!({"number":"0x0","hash":B256::repeat_byte(1),"parentHash":B256::ZERO,"timestamp":"0x0"})}
                        else {json!({"number":"0x1","hash":head.hash,"parentHash":head.parent_hash,"timestamp":format!("0x{:x}",head.timestamp)})},
                    "eth_call"=>json!("0x"),"eth_estimateGas"=>json!("0x5208"),"eth_getTransactionReceipt"=>Value::Null,
                    "eth_getBalance"|"eth_getTransactionCount"|"eth_gasPrice"=>json!("0x1"),
                    "eth_sendRawTransaction"=>{sent.fetch_add(1,Ordering::SeqCst);let raw:Bytes=serde_json::from_value(request["params"][0].clone()).unwrap();json!(keccak256(raw))},
                    other=>panic!("unexpected method {other}"),
                };
                let rate_error=method=="eth_chainId" && request["params"][0]=="retry" && retries.fetch_add(1,Ordering::SeqCst)<2;
                let failed=rate_error || method=="eth_sendRawTransaction" && reject.load(Ordering::Acquire);
                let envelope=if failed {json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":429,"message":"rate limit"}})}
                    else {json!({"jsonrpc":"2.0","id":request["id"],"result":result})};
                let body=serde_json::to_vec(&envelope).unwrap();
                if write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).is_ok() {let _=stream.write_all(&body);}
            }
        });
        Self {url,sends,reject_sends,retry_reads,stop,worker:Some(worker),head}
    }
}
impl Drop for Stub {
    fn drop(&mut self) { self.stop.store(true,Ordering::Release);let _=TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(worker)=self.worker.take() { if worker.join().is_err() && !thread::panicking() {panic!("stub thread failed");} }
    }
}
fn shared_clients(first:&Stub,second:&Stub,policy:&RpcPolicy) -> (Arc<HttpReader>,Arc<LocalTxRpc>) {
    let urls=vec![first.url.clone(),second.url.clone()];
    let transport=HttpReader::with_endpoints(&urls,policy.transport_config(1_000)).unwrap()
        .with_completion_rate_limit(policy.requests_per_second as usize).unwrap()
        .with_read_optimizations(policy.read_optimizations.clone()).unwrap();
    let bulk=Arc::new(transport.control_reader(policy.bulk_config(1_000)).unwrap());
    let control=Arc::new(bulk.control_reader(policy.control_config(1_000)).unwrap());
    let tx=Arc::new(LocalTxRpc::for_endpoints(&urls,control,"local",ChainIdentity {chain_id:31337,genesis_hash:B256::repeat_byte(1)}).unwrap());
    (bulk,tx)
}
fn assert_public_rate(arrivals:&[(Instant,String)]) {
    assert!(arrivals.len()>15);
    for window in arrivals.windows(16) {
        assert!(window[15].0.duration_since(window[0].0)>=Duration::from_secs(1),
            "more than 15 requests reached loopback within one second: {:?}",window[15].0.duration_since(window[0].0));
    }
}

#[tokio::test]
async fn shared_rpc_budget_covers_reads_receipts_simulation_estimation_and_send_across_fallbacks() {
    let arrivals=Arc::new(Mutex::new(Vec::new()));let first=Stub::new(arrivals.clone());let second=Stub::new(arrivals.clone());
    let policy=RpcPolicy::default();let (reader,tx)=shared_clients(&first,&second,&policy);
    let signer=PrivateKeySigner::from_slice(&[1u8;32]).unwrap();
    let unsigned=UnsignedTx {chain_id:31337,nonce:0,to:signer.address(),input:Bytes::new(),gas_limit:21_000,gas_price:U256::from(1)};
    let raw=unsigned.sign(&signer).await.unwrap();let mut jobs=tokio::task::JoinSet::new();
    for _ in 0..12 {let reader=reader.clone();jobs.spawn(async move {reader.rpc().request("eth_chainId",json!([])).await.unwrap();});}
    for _ in 0..6 {let tx=tx.clone();jobs.spawn(async move {tx.receipt(B256::repeat_byte(4)).await.unwrap();tx.balance(Address::repeat_byte(5)).await.unwrap();});}
    let (simulate_rpc,estimate_rpc,send_rpc)=(tx.clone(),tx.clone(),tx.clone());
    let (simulated,estimated)=(unsigned.clone(),unsigned.clone());let at=first.head;
    jobs.spawn(async move {simulate_rpc.simulate(Address::repeat_byte(5),&simulated,&at).await.unwrap();});
    jobs.spawn(async move {estimate_rpc.estimate(Address::repeat_byte(5),&estimated,&at).await.unwrap();});
    jobs.spawn(async move {assert_eq!(send_rpc.send_raw(&raw).await.unwrap(),keccak256(&raw));});
    while let Some(result)=jobs.join_next().await {result.unwrap();}
    reader.rpc().select_endpoint(1).unwrap();
    for _ in 0..4 {tx.gas_price().await.unwrap();reader.rpc().request("eth_chainId",json!([])).await.unwrap();}
    assert_eq!(first.sends.load(Ordering::SeqCst),1);assert_eq!(second.sends.load(Ordering::SeqCst),0);
    let arrivals=arrivals.lock().unwrap();assert_public_rate(&arrivals);
    for method in ["eth_call","eth_estimateGas","eth_getTransactionReceipt","eth_sendRawTransaction","eth_chainId"] {
        assert!(arrivals.iter().any(|(_,seen)|seen==method));
    }
}

#[tokio::test]
async fn retry_attempts_consume_shared_budget_but_mutation_is_not_automatically_retried() {
    let arrivals=Arc::new(Mutex::new(Vec::new()));let first=Stub::new(arrivals.clone());let second=Stub::new(arrivals.clone());
    let policy=RpcPolicy {retry_backoff_ms:0,..Default::default()};let (reader,tx)=shared_clients(&first,&second,&policy);
    let mut jobs=tokio::task::JoinSet::new();let retry_reader=reader.clone();
    jobs.spawn(async move {retry_reader.rpc().request("eth_chainId",json!(["retry"])).await.unwrap();});
    for _ in 0..16 {let tx=tx.clone();jobs.spawn(async move {tx.gas_price().await.unwrap();});}
    while let Some(result)=jobs.join_next().await {result.unwrap();}
    assert_eq!(first.retry_reads.load(Ordering::SeqCst),3);
    first.reject_sends.store(true,Ordering::Release);
    let signer=PrivateKeySigner::from_slice(&[1u8;32]).unwrap();
    let unsigned=UnsignedTx {chain_id:31337,nonce:0,to:signer.address(),input:Bytes::new(),gas_limit:21_000,gas_price:U256::from(1)};
    assert!(tx.send_raw(&unsigned.sign(&signer).await.unwrap()).await.is_err());
    assert_eq!(first.sends.load(Ordering::SeqCst),1,"transport must return the failure to durable recovery, never auto-resend");
    assert_public_rate(&arrivals.lock().unwrap());
}
