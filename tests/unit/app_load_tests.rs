//! Local HTTP load acceptance. The server models protocol getters/events; it is
//! deliberately not an EVM and cannot qualify transaction execution or a provider.
use super::*;
use std::{collections::BTreeSet, io::{BufRead, BufReader, Read, Write}, net::{TcpListener, TcpStream},
    sync::{Mutex, atomic::{AtomicBool, AtomicU64, Ordering}}, thread, time::Instant};
use alloy::{primitives::{B256, Bytes}, sol_types::{SolEvent, SolValue}};
use barterlend_state::{abi::{CreditPool, IPriceOracle}, risk::evaluate_risk};
use serde_json::{Value, json};
use async_trait::async_trait;
use barterlend_state::{RawLog,Result as StateResult};

const BLOCK_MS: u64 = 300;
const START_BLOCK: u64 = 100;

alloy::sol! {
    interface LoadMulticall3 {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result { bool success; bytes returnData; }
        function aggregate3(Call3[] calldata calls) external returns (Result[] memory returnData);
        function getBlockHash(uint256 blockNumber) external view returns (bytes32 blockHash);
    }
}

fn multicall_code() -> Bytes {
    include_str!("../../tests/fixtures/multicall3-runtime.hex").trim().parse().unwrap()
}

fn load_decode<C: SolCall>(data: &[u8]) -> C {
    let value=C::abi_decode_validate(data).expect("fixture received malformed ABI");
    assert_eq!(value.abi_encode(),data,"fixture requires exact calldata");value
}

/// Transparent instrumentation: every operation still uses the real HTTP reader
/// and its common budget. Record the oracle scope before its first header RPC,
/// which itself can fail before any eth_call reaches the server.
struct ObservedReader { inner:Arc<HttpReader>,oracle_reads:Arc<Mutex<BTreeSet<(Address,u64)>>> }
#[async_trait]
impl StateReader for ObservedReader {
    fn supports_verified_sessions(&self)->bool{self.inner.supports_verified_sessions()}
    async fn verify_log_blocks(&self,b:&[(u64,B256)],c:Commitment)->StateResult<()>{self.inner.verify_log_blocks(b,c).await}
    async fn identity(&self)->StateResult<ChainIdentity>{self.inner.identity().await}
    async fn head(&self,c:Commitment)->StateResult<BlockRef>{self.inner.head(c).await}
    async fn block(&self,n:u64,c:Commitment)->StateResult<Option<BlockRef>>{self.inner.block(n,c).await}
    async fn describe_pool(&self,s:&PoolSpec,a:&BlockRef)->StateResult<PoolDescriptor>{self.inner.describe_pool(s,a).await}
    async fn read_positions(&self,p:Address,k:&[PositionKey],a:&BlockRef)->StateResult<BTreeMap<PositionKey,PositionAmounts>>{self.inner.read_positions(p,k,a).await}
    async fn read_globals(&self,p:Address,a:&BlockRef)->StateResult<PoolRiskConfig>{self.inner.read_globals(p,a).await}
    async fn read_market_configs(&self,p:Address,m:&[u64],a:&BlockRef)->StateResult<BTreeMap<u64,MarketRiskConfig>>{self.inner.read_market_configs(p,m,a).await}
    async fn read_oracle(&self,p:&PoolDescriptor,a:&BlockRef)->StateResult<OracleStatus>{
        self.oracle_reads.lock().unwrap().insert((p.oracle,a.number));self.inner.read_oracle(p,a).await
    }
    async fn logs(&self,p:Address,f:u64,t:u64)->StateResult<Vec<RawLog>>{self.inner.logs(p,f,t).await}
    async fn market_users(&self,p:Address,m:u64,s:u64,l:u64,a:&BlockRef)->StateResult<Vec<Address>>{self.inner.market_users(p,m,s,l,a).await}
    async fn active_markets(&self,p:Address,a:&BlockRef)->StateResult<Vec<u64>>{self.inner.active_markets(p,a).await}
    async fn market_schedule(&self,p:Address,a:&BlockRef)->StateResult<barterlend_state::engine::current::MarketSchedule>{self.inner.market_schedule(p,a).await}
    async fn market_user_pages(&self,p:Address,pages:&[barterlend_state::MarketUserPage],a:&BlockRef)->StateResult<Vec<Vec<Address>>>{self.inner.market_user_pages(p,pages,a).await}
}

fn address(n: u64) -> Address { Address::from_word(B256::from(U256::from(n).to_be_bytes::<32>())) }
fn hash(n: u64) -> B256 { keccak256(format!("local-load-canonical-block-{n}")) }
fn hex(n: u64) -> String { format!("0x{n:x}") }
fn number(value: &Value) -> u64 { u64::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"),16).unwrap() }

#[derive(Clone)]
struct ObservedCall { accepted_us:u64,read_us:u64,processed_us:u64,method:String }

#[derive(Clone)]
struct LoadModel {
    manifest: LocalManifest,
    per_pool: usize,
    base_timestamp: u64,
    started: Instant,
    phase_origin: Arc<AtomicU64>,
    calls: Arc<Mutex<Vec<ObservedCall>>>,
    call_details: Arc<Mutex<Vec<Value>>>,
    faults: Arc<AtomicBool>,
    reject_next: Arc<AtomicU64>,
    statuses: Arc<Mutex<BTreeMap<u16,usize>>>,
    oracle_faults: Arc<Mutex<BTreeSet<(Address,u64)>>>,
    oracle_reads: Arc<Mutex<BTreeSet<(Address,u64)>>>,
    header_faults: Arc<Mutex<BTreeSet<u64>>>,
    unsupported: Arc<Mutex<Vec<String>>>,
    native_batches: Arc<AtomicU64>,
    external_batches: Arc<AtomicU64>,
}

impl LoadModel {
    fn head(&self) -> u64 { START_BLOCK + self.started.elapsed().as_millis() as u64 / BLOCK_MS }
    fn timestamp(&self, block: u64) -> u64 { self.base_timestamp + block.saturating_sub(START_BLOCK)*BLOCK_MS/1000 }
    fn block(&self, block: u64) -> BlockRef { BlockRef { number:block,hash:hash(block),parent_hash:if block==0 {B256::ZERO}else{hash(block-1)},timestamp:self.timestamp(block),commitment:Commitment::Latest } }
    fn seconds(&self, block: u64) -> u64 {
        let origin=self.phase_origin.load(Ordering::SeqCst);
        if origin==0 { 0 } else { block.saturating_sub(origin)*BLOCK_MS/1000 }
    }
    fn market(&self, index: usize) -> u64 {
        // Exactly half the initial positions in each of two debt-bearing markets.
        self.base_timestamp + if index%2==1 { 95 } else { 95_000 }
    }
    fn active_markets(&self, at:u64)->Vec<u64> {
        let elapsed=self.timestamp(at)-self.base_timestamp;
        BTreeSet::from([self.base_timestamp+(elapsed/95+1)*95,
            self.base_timestamp+(elapsed/95_000+1)*95_000]).into_iter().collect()
    }
    fn key(&self,pool:Address,index:usize)->PositionKey { PositionKey { pool,market:self.market(index),borrower:address(100_000+index as u64) } }
    fn amounts(&self,index:usize,at:u64)->PositionAmounts {
        let burst=self.seconds(at)>=20;
        if index>self.per_pool || index==self.per_pool && !burst { return PositionAmounts::default(); }
        if burst && index==2 { return PositionAmounts::default(); }
        let origin=self.phase_origin.load(Ordering::SeqCst);
        let elapsed=at.saturating_sub(origin)/2;
        let first=index.saturating_sub(4) as u64+1;
        let updates=if origin!=0&&index>=4&&index<self.per_pool&&elapsed>=first {
            (elapsed-first)/(self.per_pool-4) as u64+1
        } else {0};
        PositionAmounts { debt:if burst && index==0 {0}else{100},
            collateral:if burst && index==1 {100}else if index%1000==3 {150}else{1000+10*u128::from(updates%2)} }
    }
    fn globals(&self)->PoolRiskConfig { PoolRiskConfig { liquidation_ltv:U256::from(800_000_000_000_000_000u128),stale_window:20,min_debt_remaining:10 } }
    fn market_config(&self)->MarketRiskConfig { MarketRiskConfig {grace_period:0,discount_min:U256::from(50_000_000_000_000_000u128),discount_max:U256::from(200_000_000_000_000_000u128)} }
    // A second sustained price shock reaches backpressure even when a short
    // fault-window shock expires before the execution loop samples the queue.
    fn price(&self,at:u64)->Price {
        let seconds=self.seconds(at);
        Price { numerator:U256::from(if (40..70).contains(&seconds) || (500..650).contains(&seconds) {1}else{2}),denominator:U256::from(if (40..70).contains(&seconds) || (500..650).contains(&seconds) {20}else{2}),
            updated_at:U256::from(if (75..95).contains(&seconds) {self.timestamp(at).saturating_sub(60)}else{self.timestamp(at)}) }
    }
    fn oracle(&self,at:u64)->OracleStatus {
        let price=self.price(at);
        if (75..95).contains(&self.seconds(at)) {
            OracleStatus::Unavailable(OracleFailure::Stale {updated_at:price.updated_at,stale_window:20,block_timestamp:self.timestamp(at)})
        } else { OracleStatus::Ready(price) }
    }
    fn log<E: SolEvent>(&self,pool:Address,block:u64,index:u64,event:E)->Value {
        let event=event.encode_log_data();
        json!({"address":pool,"topics":event.topics(),"data":event.data,"blockNumber":hex(block),"blockHash":hash(block),
            "transactionHash":keccak256(format!("{pool}-{block}-{index}")),"logIndex":hex(index),"removed":false})
    }
    fn logs(&self,pool:Address,from:u64,to:u64)->Vec<Value> {
        let mut logs=Vec::new();
        if from<=START_BLOCK && to>=START_BLOCK {
            // Cold census discovers every borrower through actual ABI event encoding.
            for i in 0..self.per_pool {
                let key=self.key(pool,i);
                logs.push(self.log(pool,START_BLOCK,i as u64,CreditPool::Borrowed {account:key.borrower,market:key.market,assetsBorrowed:100,debtIncurred:100,endAPR:U256::ZERO}));
            }
        }
        let origin=self.phase_origin.load(Ordering::SeqCst);
        let burst=origin+20_000_u64.div_ceil(BLOCK_MS);
        if origin!=0 && from<=burst && to>=burst {
            let a=self.key(pool,0);let b=self.key(pool,1);let c=self.key(pool,2);let d=self.key(pool,self.per_pool);
            logs.push(self.log(pool,burst,0,CreditPool::DebtRepaid {payer:address(99),account:a.borrower,market:a.market,amount:100}));
            logs.push(self.log(pool,burst,1,CreditPool::CollateralWithdrawn {account:b.borrower,recipient:b.borrower,market:b.market,amount:900}));
            logs.push(self.log(pool,burst,2,CreditPool::DebtRepaid {payer:address(99),account:c.borrower,market:c.market,amount:100}));
            logs.push(self.log(pool,burst,3,CreditPool::CollateralWithdrawn {account:c.borrower,recipient:c.borrower,market:c.market,amount:1000}));
            logs.push(self.log(pool,burst,4,CreditPool::Borrowed {account:d.borrower,market:d.market,assetsBorrowed:100,debtIncurred:100,endAPR:U256::ZERO}));
            logs.push(self.log(pool,burst,5,CreditPool::CollateralDeposited {account:d.borrower,market:d.market,amount:1000}));
        }
        if origin!=0 {
            // One collateral change per block per pool, rotating across existing
            // borrowers. Values can be independently reconstructed at any anchor.
            for block in from.max(origin+1)..=to {
                if (block-origin)%2!=0 {continue;}
                let index=4+(((block-origin)/2-1)%(self.per_pool-4) as u64) as usize;
                // Keep the deliberately thin collateral position unchanged.
                if index%1000==3 {continue;}
                let key=self.key(pool,index);
                let depositing=(((block-origin)/2-1)/(self.per_pool-4) as u64)%2==0;
                if depositing {
                    logs.push(self.log(pool,block,6,CreditPool::CollateralDeposited {account:key.borrower,market:key.market,amount:10}));
                } else {
                    logs.push(self.log(pool,block,6,CreditPool::CollateralWithdrawn {account:key.borrower,recipient:key.borrower,market:key.market,amount:10}));
                }
            }
        }
        logs
    }
    fn call(&self,to:Address,data:&[u8],at:u64,depth:usize)->std::result::Result<Bytes,String> {
        assert!(data.len()>=4 && depth<=1,"unbounded or malformed load call");
        let selector=&data[..4];
        if let Some(config)=&self.manifest.rpc.multicall3 {
            if to==config.address {
                if selector==LoadMulticall3::getBlockHashCall::SELECTOR {
                    let number=load_decode::<LoadMulticall3::getBlockHashCall>(data).blockNumber.to::<u64>();
                    return Ok((if number<at && at-number<=256 {hash(number)}else{B256::ZERO}).abi_encode().into());
                }
                if selector==LoadMulticall3::aggregate3Call::SELECTOR && depth==0 {
                    let call=load_decode::<LoadMulticall3::aggregate3Call>(data);
                    assert!(!call.calls.is_empty()&&call.calls.len()<=config.max_calls);
                    self.external_batches.fetch_add(1,Ordering::Relaxed);
                    let mut values=Vec::new();
                    for call in call.calls {
                        match self.call(call.target,&call.callData,at,depth+1) {
                            Ok(return_data)=>values.push(LoadMulticall3::Result{success:true,returnData:return_data}),
                            Err(error) if !call.allowFailure=>return Err(error),
                            Err(_)=>values.push(LoadMulticall3::Result{success:false,returnData:Bytes::new()}),
                        }
                    }
                    return Ok(LoadMulticall3::aggregate3Call::abi_encode_returns(&values).into());
                }
            }
        }
        // An explicit, expected preparation failure. This HTTP model never
        // fabricates a successful executor simulation or financial receipt.
        if to==self.manifest.executor && [LiquidationExecutor::ownerCall::SELECTOR,LiquidationExecutor::operatorsCall::SELECTOR,
            LiquidationExecutor::routerCall::SELECTOR,LiquidationExecutor::factoryCall::SELECTOR,
            LiquidationExecutor::poolConfigCall::SELECTOR].iter().any(|s|selector==s) {
            return Err("synthetic load: financial execution deliberately unavailable".into());
        }
        let pool=self.manifest.pools.values().find(|p|p.spec.address==to);
        if let Some(pool)=pool {
            if selector==CreditPool::multicallCall::SELECTOR && depth==0 {
                let call=load_decode::<CreditPool::multicallCall>(data);
                assert!(!call.data.is_empty()&&call.data.len()<=1024);
                self.native_batches.fetch_add(1,Ordering::Relaxed);
                let values=call.data.iter().map(|data|self.call(to,data,at,depth+1)).collect::<std::result::Result<Vec<_>,_>>()?;
                return Ok(CreditPool::multicallCall::abi_encode_returns(&values).into());
            }
            if selector==CreditPool::versionCall::SELECTOR {return Ok(U256::from(5).abi_encode().into());}
            if selector==CreditPool::assetCall::SELECTOR {return Ok(pool.asset.address.abi_encode().into());}
            if selector==CreditPool::collateralCall::SELECTOR {return Ok(pool.collateral.address.abi_encode().into());}
            if selector==CreditPool::priceOracleCall::SELECTOR {return Ok(pool.oracle.abi_encode().into());}
            if selector==CreditPool::getActiveMarketsCall::SELECTOR {return Ok(self.active_markets(at).abi_encode().into());}
            if selector==CreditPool::getMarketsConfigCall::SELECTOR {
                // The typed ABI distinguishes uint8[3] from Rust's bytes3 encoding.
                return Ok(CreditPool::getMarketsConfigCall::abi_encode_returns(&CreditPool::MarketsConfig {
                    start:self.base_timestamp,durations:[95,95_000,95_000],active:[1;3],extensions:[0;3],
                }).into());
            }
            if selector==CreditPool::getLiquidationLTVCall::SELECTOR {return Ok(self.globals().liquidation_ltv.abi_encode().into());}
            if selector==CreditPool::getStalePriceWindowCall::SELECTOR {return Ok(self.globals().stale_window.abi_encode().into());}
            if selector==CreditPool::getMinDebtRemainingAfterLiquidationCall::SELECTOR {return Ok(self.globals().min_debt_remaining.abi_encode().into());}
            if selector==CreditPool::getLiquidationGracePeriodCall::SELECTOR {return Ok(self.market_config().grace_period.abi_encode().into());}
            if selector==CreditPool::getLiquidationDiscountMinCall::SELECTOR {return Ok(self.market_config().discount_min.abi_encode().into());}
            if selector==CreditPool::getLiquidationDiscountMaxCall::SELECTOR {return Ok(self.market_config().discount_max.abi_encode().into());}
            if selector==CreditPool::debtOfCall::SELECTOR {
                let call=load_decode::<CreditPool::debtOfCall>(data);let index=U256::from_be_slice(call.account.as_slice()).to::<usize>()-100_000;
                assert_eq!(call.market,self.market(index));return Ok(self.amounts(index,at).debt.abi_encode().into());
            }
            if selector==CreditPool::collateralOfCall::SELECTOR {
                let call=load_decode::<CreditPool::collateralOfCall>(data);let index=U256::from_be_slice(call.account.as_slice()).to::<usize>()-100_000;
                assert_eq!(call.market,self.market(index));return Ok(self.amounts(index,at).collateral.abi_encode().into());
            }
            if selector==CreditPool::getMarketUsersCall::SELECTOR {
                let call=load_decode::<CreditPool::getMarketUsersCall>(data);let count=self.per_pool+usize::from(self.seconds(at)>=20);
                let values:Vec<_>=(0..count).filter(|i|self.market(*i)==call.market).skip(call.start.to::<usize>()).take(call.limit.to::<usize>()).map(|i|self.key(to,i).borrower).collect();
                return Ok(values.abi_encode().into());
            }
        }
        if let Some(pool)=self.manifest.pools.values().find(|p|p.oracle==to) {
            self.oracle_reads.lock().unwrap().insert((to,at));
            if selector==IPriceOracle::sourceCall::SELECTOR {return Ok(pool.collateral.address.abi_encode().into());}
            if selector==IPriceOracle::targetCall::SELECTOR {return Ok(pool.asset.address.abi_encode().into());}
            if selector==IPriceOracle::getPriceCall::SELECTOR {let p=self.price(at);return Ok((p.numerator,p.denominator,p.updated_at).abi_encode().into());}
        }
        if self.manifest.v3.allowed_tokens.contains(&to) && selector==&keccak256("decimals()")[..4] {
            return Ok(18u64.abi_encode().into());
        }
        let error=format!("unsupported load call {to} {:?}",selector);
        self.unsupported.lock().unwrap().push(error.clone());Err(error)
    }

    fn handle(&self,request:&Value,accepted_us:u64,read_us:u64)->(u16,Value) {
        let method=request["method"].as_str().unwrap().to_owned();
        let processed_us=self.started.elapsed().as_micros() as u64;
        self.calls.lock().unwrap().push(ObservedCall{accepted_us,read_us,processed_us,method:method.clone()});
        let mut detail=json!({"elapsed_ms":self.started.elapsed().as_millis(),"phase_seconds":self.seconds(self.head()),"method":method,
            "tcp_accepted_us":accepted_us,"http_request_read_us":read_us,"processing_started_us":processed_us,
            "to":request["params"][0].get("to"),"selector":request["params"][0].get("data").and_then(Value::as_str).and_then(|data|data.get(..10)),
            "block":if method=="eth_call"{request["params"][1].clone()}else if method=="eth_getBlockByNumber"{request["params"][0].clone()}else{Value::Null}});
        let seconds=self.seconds(self.head());
        let failure=if self.faults.load(Ordering::SeqCst)&&(55..60).contains(&seconds){Some(503)}
            else if self.reject_next.fetch_update(Ordering::SeqCst,Ordering::SeqCst,|n|(n>0).then(||n-1)).is_ok(){Some(429)}else{None};
        detail["http_status"]=json!(failure.unwrap_or(200));self.call_details.lock().unwrap().push(detail);
        if method=="eth_call"{
            let to:Address=serde_json::from_value(request["params"][0]["to"].clone()).unwrap();
            if self.manifest.pools.values().any(|pool|pool.oracle==to){self.oracle_reads.lock().unwrap().insert((to,number(&request["params"][1])));}
        }
        if let Some(status)=failure{
            *self.statuses.lock().unwrap().entry(status).or_default()+=1;
            if method=="eth_getBlockByNumber"&&request["params"][0].as_str().is_some_and(|block|block.starts_with("0x")){
                self.header_faults.lock().unwrap().insert(number(&request["params"][0]));
            }
            if method=="eth_call"{
                let to:Address=serde_json::from_value(request["params"][0]["to"].clone()).unwrap();
                if self.manifest.pools.values().any(|pool|pool.oracle==to){self.oracle_faults.lock().unwrap().insert((to,number(&request["params"][1])));}
            }
            return(status,json!({}));
        }
        let params=&request["params"];
        let value=match method.as_str() {
            "eth_chainId"=>json!(hex(self.manifest.chain.chain_id)),
            "eth_getBlockByNumber"=>{
                let at=match params[0].as_str().unwrap(){"latest"|"safe"|"finalized"=>self.head(),_=>number(&params[0])};
                if at>self.head() {Value::Null}else{let b=self.block(at);json!({"number":hex(at),"hash":b.hash,"parentHash":b.parent_hash,"timestamp":hex(b.timestamp)})}
            },
            "eth_getCode"=>{
                let to:Address=serde_json::from_value(params[0].clone()).unwrap();
                if self.manifest.rpc.multicall3.as_ref().is_some_and(|c|c.address==to){json!(multicall_code())}else{json!("0x6000")}
            },
            "eth_getLogs"=>{
                let from=number(&params[0]["fromBlock"]);let to=number(&params[0]["toBlock"]);
                if to-from+1>1000 {return(200,json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32005,"message":"block range limit 1000"}}));}
                let pool:Address=serde_json::from_value(params[0]["address"].clone()).unwrap();json!(self.logs(pool,from,to))
            },
            "eth_getTransactionReceipt"=>Value::Null,
            "eth_getTransactionCount"=>json!("0x0"),
            "eth_getBalance"=>json!("0x8ac7230489e80000"),
            "eth_gasPrice"=>json!("0x77359400"),
            "eth_call"=>{
                let at=number(&params[1]);let bytes:Bytes=serde_json::from_value(params[0]["data"].clone()).unwrap();
                let to:Address=serde_json::from_value(params[0]["to"].clone()).unwrap();
                match self.call(to,&bytes,at,0) {
                    Ok(bytes)=>json!(bytes),
                    Err(message)=>return(200,json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":3,"message":message}})),
                }
            },
            _=>panic!("unexpected local load RPC {method}"),
        };
        (200,json!({"jsonrpc":"2.0","id":request["id"],"result":value}))
    }
}

struct LoadServer {url:String,stop:Arc<AtomicBool>,task:Option<thread::JoinHandle<()>>}
impl LoadServer {
    fn start(model:LoadModel)->Self {
        let latency_ms:u64=std::env::var("BARTERLEND_LOAD_LATENCY_MS").unwrap_or_else(|_|"100".into()).parse().unwrap();assert!(matches!(latency_ms,15|100));
        let mut sequence=0u64;
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let addr=listener.local_addr().unwrap();listener.set_nonblocking(true).unwrap();
        let stop=Arc::new(AtomicBool::new(false));let done=stop.clone();
        let task=thread::spawn(move||{
            let mut workers:Vec<thread::JoinHandle<()>>=Vec::new();
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream,_))=>{
                        let accepted_us=model.started.elapsed().as_micros() as u64;
                        if done.load(Ordering::SeqCst){break;}
                        stream.set_nonblocking(false).unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                        let model=model.clone();sequence+=1;let delay=if latency_ms==100 && sequence%10==0 {500}else{latency_ms};
                        workers.push(thread::spawn(move||{
                            let Some(request)=read_request(&stream)else{return;};
                            let read_us=model.started.elapsed().as_micros() as u64;
                            // Deterministic 100ms baseline with one 500ms tail per ten requests.
                            thread::sleep(Duration::from_millis(delay));
                            let(status,value)=model.handle(&request,accepted_us,read_us);let body=serde_json::to_vec(&value).unwrap();
                            let _=write!(stream,"HTTP/1.1 {status} load\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());let _=stream.write_all(&body);
                        }));
                    },
                    Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>thread::sleep(Duration::from_millis(1)),
                    Err(e)=>panic!("load accept: {e}"),
                }
                let mut i=0;while i<workers.len(){if workers[i].is_finished(){workers.swap_remove(i).join().unwrap();}else{i+=1;}}
            }
            for worker in workers{worker.join().unwrap();}
        });
        Self{url:format!("http://{addr}"),stop,task:Some(task)}
    }
}
impl Drop for LoadServer{fn drop(&mut self){self.stop.store(true,Ordering::SeqCst);let _=TcpStream::connect(self.url.trim_start_matches("http://"));if let Some(task)=self.task.take(){if task.join().is_err()&&!thread::panicking(){panic!("load server panicked");}}}}
fn read_request(stream:&TcpStream)->Option<Value>{
    let mut reader=BufReader::new(stream.try_clone().ok()?);let mut line=String::new();let mut len=None;
    if reader.read_line(&mut line).ok()?==0{return None;}assert!(line.starts_with("POST "));
    loop{line.clear();if reader.read_line(&mut line).ok()?==0{return None;}
        if line=="\r\n"{break;}
        if let Some(v)=line.to_ascii_lowercase().strip_prefix("content-length:"){len=Some(v.trim().parse::<usize>().ok()?);}}
    let length=len?;assert!(length<1_048_576);let mut body=vec![0;length];reader.read_exact(&mut body).ok()?;serde_json::from_slice(&body).ok()
}

fn cpu_rss()->(f64,u64){
    let mut usage=std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // getrusage writes a plain OS struct and retains no pointer.
    let result=unsafe{libc::getrusage(libc::RUSAGE_SELF,usage.as_mut_ptr())};assert_eq!(result,0);
    let usage=unsafe{usage.assume_init()};let cpu=usage.ru_utime.tv_sec as f64+usage.ru_utime.tv_usec as f64/1e6+usage.ru_stime.tv_sec as f64+usage.ru_stime.tv_usec as f64/1e6;
    #[cfg(target_os="macos")]let rss=usage.ru_maxrss as u64;
    #[cfg(not(target_os="macos"))]let rss=usage.ru_maxrss as u64*1024;
    (cpu,rss)
}
fn percentile(mut values:Vec<u64>,p:usize)->u64{if values.is_empty(){return 0;}values.sort_unstable();values[((values.len()-1)*p)/100]}
fn rolling_second_max(mut times:Vec<u64>)->usize{
    times.sort_unstable();let mut left=0;let mut maximum=0;
    for(right,time)in times.iter().enumerate(){while times[left]+1_000_000<=*time{left+=1;}maximum=maximum.max(right-left+1);}
    maximum
}
fn actionable(view:&PoolView,head:BlockRef,max_age_blocks:u64)->bool {
    view.verified&&view.snapshot.as_ref().is_some_and(|snapshot|head.number.checked_sub(snapshot.as_of.number).is_some_and(|lag|lag<=max_age_blocks)&&match &snapshot.oracle{
        OracleStatus::Ready(price)=>price.updated_at<=U256::from(head.timestamp)&&price.updated_at.checked_add(U256::from(snapshot.globals.stale_window)).is_some_and(|expiry|U256::from(head.timestamp)<expiry),
        OracleStatus::Unavailable(_)=>false,
    })
}
fn write_report(report:&Value){
    let mut compact=report.clone();compact.as_object_mut().unwrap().remove("samples");
    compact.as_object_mut().unwrap().remove("rpc_calls");
    eprintln!("LOAD_RESULT {}",serde_json::to_string(&compact).unwrap());
    if let Ok(path)=std::env::var("BARTERLEND_LOAD_REPORT"){std::fs::write(path,serde_json::to_vec_pretty(report).unwrap()).unwrap();}
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="local sustained HTTP load; run tests/runners/test-local-load.sh, never a public endpoint"]
async fn http_300ms_many_pools_positions(){
    let pools:usize=std::env::var("BARTERLEND_LOAD_POOLS").unwrap_or_else(|_|"4".into()).parse().unwrap();
    let positions:usize=std::env::var("BARTERLEND_LOAD_POSITIONS").unwrap_or_else(|_|"1200".into()).parse().unwrap();
    let cold=true;
    let duration:u64=std::env::var("BARTERLEND_LOAD_SECONDS").unwrap_or_else(|_|"750".into()).parse().unwrap();
    let rpc_limit:usize=std::env::var("BARTERLEND_LOAD_RPS").unwrap_or_else(|_|"15".into()).parse().unwrap();assert!((1..=1000).contains(&rpc_limit));
    assert!((1..=8).contains(&pools)&&positions>=pools*16&&positions%(pools*2)==0);assert!((120..=900).contains(&duration));
    let mut manifest:LocalManifest=serde_json::from_str(include_str!("../../examples/local-manifest.json")).unwrap();
    manifest.poll_ms=1000;manifest.queue_capacity=1000;
    manifest.v3.limits.max_age_blocks=200;manifest.v3.limits.quote_ttl_seconds=60;manifest.planner.ttl_seconds=60;
    manifest.chain.genesis_hash=hash(0);manifest.v3.chain=manifest.chain;manifest.sdk_log_chunk_size=1000;
    // The production 15rps defaults must also apply to this local endpoint.
    // Deserialize through the public manifest shape instead of a test-only limiter.
    let mut raw=serde_json::to_value(&manifest).unwrap();raw["rpc"]=json!({"requests_per_second":rpc_limit});
    manifest=serde_json::from_value(raw).expect("RPC limits must be configurable in the manifest");
    manifest.rpc.multicall3=Some(Multicall3Config {address:address(900),code_hash:keccak256(multicall_code()),..Multicall3Config::canonical()});
    manifest.rpc.read_optimizations.batch_log_hashes=true;
    manifest.sync.current_state=Some(CurrentStateConfig::default());
    manifest.sync.reconcile_user_page_size=128;
    let template=manifest.pools.values().next().unwrap().clone();manifest.pools.clear();
    for i in 0..pools{
        let mut pool=template.clone();pool.spec.address=address(1000+i as u64);pool.oracle=address(2000+i as u64);pool.spec.deployment_block=START_BLOCK;pool.spec.deployment_block_hash=Some(hash(START_BLOCK));pool.code_hash=keccak256([0x60,0]);
        manifest.pools.insert(format!("load-{i}"),pool);
    }
    let base_timestamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let model=LoadModel{manifest:manifest.clone(),per_pool:positions/pools,base_timestamp,started:Instant::now(),phase_origin:Arc::new(AtomicU64::new(0)),calls:Arc::new(Mutex::new(Vec::new())),call_details:Arc::new(Mutex::new(Vec::new())),faults:Arc::new(AtomicBool::new(true)),reject_next:Arc::new(AtomicU64::new(0)),statuses:Arc::new(Mutex::new(BTreeMap::new())),oracle_faults:Arc::new(Mutex::new(BTreeSet::new())),oracle_reads:Arc::new(Mutex::new(BTreeSet::new())),header_faults:Arc::new(Mutex::new(BTreeSet::new())),unsupported:Arc::new(Mutex::new(Vec::new())),native_batches:Arc::new(AtomicU64::new(0)),external_batches:Arc::new(AtomicU64::new(0))};
    let server=LoadServer::start(model.clone());manifest.rpc_url=server.url.clone();manifest.validate().unwrap();
    let reader=Arc::new(state_reader(&manifest).unwrap());
    let observed_reader=Arc::new(ObservedReader{inner:reader.clone(),oracle_reads:model.oracle_reads.clone()});
    let control=Arc::new(reader.control_reader(manifest.rpc.control_config(manifest.sdk_log_chunk_size)).unwrap());
    let rpc=Arc::new(LocalTxRpc::for_endpoints(&rpc_endpoints(&manifest),control.clone(),&manifest.profile,manifest.chain).unwrap());
    let routes=Arc::new(SwapRouteProvider::new(control.clone(),manifest.v3.clone()).unwrap());let native=ConfiguredNativeCostOracle::new(routes.clone(),manifest.executor,&[]).unwrap();let routes=Arc::new(ProviderRegistry::new(vec![routes]).unwrap());
    let monitors=Arc::new(EndpointHealth::new(manifest.recovery.clone(),false,1));
    let (views_tx,mut views_rx)=watch::channel(BTreeMap::new());let(mut stop_tx,stop_rx)=watch::channel(false);
    let ingest_manifest=manifest.clone();let ingest_reader=observed_reader.clone();let ingest_views=views_tx.clone();
    let mut ingestion=Some(tokio::spawn(async move{ingest_pools(ingest_manifest,ingest_reader,false,ingest_views,stop_rx).await}));
    let (shutdown_tx,shutdown_rx)=tokio::sync::oneshot::channel::<()>();let mut loop_views=views_rx.clone();let loop_manifest=manifest.clone();let loop_rpc=rpc.clone();
    let mut diagnostics=Diagnostics::new(&manifest,false);let health=diagnostics.health();
    let app=tokio::spawn(async move{
        let shutdown=async{let _=shutdown_rx.await;Ok(())};tokio::pin!(shutdown);
        let mut report=RunReport::default();let mut worker=None;
        run_loop(&loop_manifest,&RunOptions{live:false,cycles:0},None,&control,&routes,&native,&loop_rpc,&mut worker,&mut loop_views,&mut diagnostics,&mut report,&monitors,&crate::server::Progress::default(),None,shutdown.as_mut()).await
    });
    let extra_rpc=rpc.clone();let operator=manifest.operator;
    let extra=tokio::spawn(async move{let mut count=0;loop{let _=match count%4{0=>extra_rpc.gas_price().await.map(|_|()),1=>extra_rpc.balance(operator).await.map(|_|()),2=>extra_rpc.nonce(operator,false).await.map(|_|()),_=>extra_rpc.receipt(B256::repeat_byte(2)).await.map(|_|())};count+=1;tokio::time::sleep(Duration::from_secs(2)).await;}});
    let started=Instant::now();let (cpu_start,_)=cpu_rss();let mut ready_at=None;let mut lags=Vec::new();let mut steady_lags=Vec::new();let mut samples=Vec::new();let mut last_block=BTreeMap::new();let mut saw_stale=BTreeSet::new();let mut saw_low=BTreeSet::new();let mut saw_maturity=BTreeSet::new();let mut correctness_errors=Vec::new();let mut stop_reason=None;let mut progress_bucket=0;
    let mut restart_started:Option<(Instant,u64)>=None;let mut restart_ready_ms=None;let mut restart_verified_ms=None;let mut outage_recovered_seconds=None;let mut burst_detected_seconds=BTreeMap::new();let mut max_queue=0usize;let mut peak_eligible=0usize;let mut unavailable_pool_ms=0u64;let mut last_observation=Instant::now();
    let mut steady_start=None;let mut steady_available_ms=BTreeMap::<Address,u64>::new();let mut steady_unavailable_ms=BTreeMap::<Address,u64>::new();let mut steady_anchors=BTreeMap::<Address,BTreeSet<u64>>::new();let mut oracle_rpc_unavailable=0usize;
    let mut steady_actionable_ms=BTreeMap::<Address,u64>::new();let mut steady_nonactionable_ms=BTreeMap::<Address,u64>::new();
    loop {
        if !model.unsupported.lock().unwrap().is_empty(){stop_reason=Some("unsupported_fixture_call");break;}
        if ready_at.is_none()&&started.elapsed()>Duration::from_secs(90){stop_reason=Some("startup_timeout");break;}
        if started.elapsed()>Duration::from_secs(duration+150){stop_reason=Some("startup_or_completion_timeout");break;}
        let views=views_rx.borrow_and_update().clone();
        let observation_ms=last_observation.elapsed().as_millis() as u64;last_observation=Instant::now();
        if ready_at.is_some(){unavailable_pool_ms+=observation_ms*(pools-views.values().filter(|v|v.verified).count()) as u64;}
        if ready_at.is_none() && views.len()==pools && views.values().all(|v|actionable(v,model.block(model.head()),manifest.v3.limits.max_age_blocks)){
            ready_at=Some(started.elapsed().as_millis() as u64);model.phase_origin.store(model.head(),Ordering::SeqCst);model.reject_next.store(2,Ordering::SeqCst);
        }
        let phase=model.seconds(model.head());
        peak_eligible=peak_eligible.max(views.values().filter(|v|v.verified).filter_map(|v|v.snapshot.as_ref()).map(|s|s.positions.values().filter(|p|matches!(p.risk,RiskStatus::Ready(r)if r.execution_ready)).count()).sum());
        if phase>=100 && restart_started.is_none(){
            let restart_time=Instant::now();let restart_block=model.head();
            views_tx.send_modify(|views|for view in views.values_mut(){view.verified=false;view.last_error=Some("load_restart");});
            let _=stop_tx.send(true);let old=ingestion.take().unwrap();
            assert!(matches!(tokio::time::timeout(Duration::from_secs(30),old).await,Ok(Ok(Ok(())))),"ingestion failed to stop during restart");
            let(tx,rx)=watch::channel(false);stop_tx=tx;
            let m=manifest.clone();let r=observed_reader.clone();let v=views_tx.clone();
            restart_started=Some((restart_time,restart_block));
            ingestion=Some(tokio::spawn(async move{ingest_pools(m,r,false,v,rx).await}));
            // `views` was captured before invalidation; only a new watch read
            // may certify that the newly created workers have recovered.
            continue;
        }
        if restart_ready_ms.is_none(){if let Some((time,block))=restart_started{if views.len()==pools&&views.values().all(|v|v.verified&&v.snapshot.as_ref().is_some_and(|s|s.as_of.number>=block)){
            restart_verified_ms.get_or_insert(time.elapsed().as_millis()as u64);
            if views.values().all(|view|actionable(view,model.block(model.head()),manifest.v3.limits.max_age_blocks)){restart_ready_ms=Some(time.elapsed().as_millis()as u64);steady_start=Some(phase.max(125));}
        }}}
        let steady=steady_start.is_some_and(|start|phase>=start);
        if phase>=60 && outage_recovered_seconds.is_none() && views.len()==pools && views.values().all(|v|v.verified&&v.snapshot.as_ref().is_some_and(|s|model.seconds(s.as_of.number)>=60)){outage_recovered_seconds=Some(phase-60);}
        if let Some(h)=health.read().unwrap().as_ref(){max_queue=max_queue.max(h.queue_len);}
        let bucket=started.elapsed().as_secs()/15;
        if bucket>progress_bucket{progress_bucket=bucket;eprintln!("load progress pools={pools} positions={positions} elapsed={}s phase={phase}s ready={ready_at:?} requests={}",started.elapsed().as_secs(),model.calls.lock().unwrap().len());}
        for(pool,view)in &views{
            if steady{let counter=if view.verified{&mut steady_available_ms}else{&mut steady_unavailable_ms};*counter.entry(*pool).or_default()+=observation_ms;}
            let head=model.block(model.head());
            let actionable=actionable(view,head,manifest.v3.limits.max_age_blocks);
            if steady{let counter=if actionable{&mut steady_actionable_ms}else{&mut steady_nonactionable_ms};*counter.entry(*pool).or_default()+=observation_ms;}
            if let Some(snapshot)=&view.snapshot{
                let lag=model.head().saturating_sub(snapshot.as_of.number);
                if ready_at.is_some(){lags.push(lag);}
                if steady{steady_lags.push(lag);if view.verified{steady_anchors.entry(*pool).or_default().insert(snapshot.as_of.number);}}
                // Becoming verified is a distinct observation even at the same anchor.
                let observation=(snapshot.as_of.number,view.verified);
                if last_block.insert(*pool,observation)==Some(observation){continue;}
                let seconds=model.seconds(snapshot.as_of.number);
                if view.verified{
                    if seconds>=20{burst_detected_seconds.entry(pool.to_string()).or_insert(phase.saturating_sub(20));}
                    let mut oracle=model.oracle(snapshot.as_of.number);
                    let oracle_key=(snapshot.pool.oracle,snapshot.as_of.number);
                    // An oracle read also validates the pinned canonical header.
                    // Accept fail-closed RPC status only with observed failure of
                    // this oracle call or of this exact anchor after an oracle read.
                    let oracle_rpc_failed=model.oracle_faults.lock().unwrap().contains(&oracle_key)
                        || model.header_faults.lock().unwrap().contains(&snapshot.as_of.number)&&model.oracle_reads.lock().unwrap().contains(&oracle_key);
                    if matches!(snapshot.oracle,OracleStatus::Unavailable(OracleFailure::Rpc(_))) && oracle_rpc_failed{
                        oracle=snapshot.oracle.clone();oracle_rpc_unavailable+=1;
                        if snapshot.positions.values().any(|p|matches!(p.risk,RiskStatus::Ready(r)if r.execution_ready)){correctness_errors.push(format!("execution_ready after oracle RPC failure {pool}"));}
                    }
                    if snapshot.oracle!=oracle{correctness_errors.push(format!("oracle mismatch {pool} block{}",snapshot.as_of.number));}
                    if(75..95).contains(&seconds){saw_stale.insert(*pool);}
                    if((40..70).contains(&seconds) || (500..650).contains(&seconds))&&matches!(oracle,OracleStatus::Ready(_)){saw_low.insert(*pool);}
                    if snapshot.as_of.timestamp>=model.market(9) && snapshot.positions.get(&model.key(*pool,9)).is_some_and(|p|matches!(p.risk,RiskStatus::Ready(r)if r.eligible&&r.health_factor>U256::from(1_000_000_000_000_000_000u128))){saw_maturity.insert(*pool);}
                    let count=model.per_pool+usize::from(seconds>=20);
                    let expected_count=count-usize::from(seconds>=20);
                    if snapshot.positions.len()!=expected_count{correctness_errors.push(format!("position count {} != {expected_count} {pool}",snapshot.positions.len()));}
                    for index in 0..count{
                        let key=model.key(*pool,index);let amounts=model.amounts(index,snapshot.as_of.number);let record=snapshot.positions.get(&key);
                        if amounts==PositionAmounts::default(){if record.is_some(){correctness_errors.push("closed position retained".into());}continue;}
                        if !record.is_some_and(|p|p.amounts==amounts && p.risk==evaluate_risk(&amounts,key.market,&model.globals(),&model.market_config(),&oracle,snapshot.as_of.timestamp)){
                            correctness_errors.push(format!("position mismatch {pool} index{index} block{}",snapshot.as_of.number));break;
                        }
                    }
                }
                samples.push(json!({"elapsed_ms":started.elapsed().as_millis(),"phase_seconds":phase,"steady":steady,"pool":pool,"verified":view.verified,"actionable":actionable,"block":snapshot.as_of.number,"lag_blocks":lag,"positions":snapshot.positions.len(),"oracle":snapshot.oracle,"last_error":view.last_error}));
            }
        }
        if ready_at.is_some()&&phase>=duration&&steady_start.is_some_and(|start|phase>=start+30) {break;}
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let final_views=views_rx.borrow().clone();let final_head=model.head();let final_verified=final_views.len()==pools&&final_views.values().all(|v|v.verified);let _=shutdown_tx.send(());let _=stop_tx.send(true);extra.abort();let _=extra.await;
    let app_result=tokio::time::timeout(Duration::from_secs(20),app).await;
    let ingest_result=tokio::time::timeout(Duration::from_secs(20),ingestion.take().unwrap()).await;
    let (cpu_end,rss)=cpu_rss();let calls=model.calls.lock().unwrap().clone();let mut counts=BTreeMap::new();
    let max_rps=rolling_second_max(calls.iter().map(|call|call.read_us).collect());
    let max_processing_rps=rolling_second_max(calls.iter().map(|call|call.processed_us).collect());
    let max_tcp_accept_rps=rolling_second_max(calls.iter().map(|call|call.accepted_us).collect());
    for call in &calls{*counts.entry(call.method.clone()).or_insert(0usize)+=1;}
    let final_lag=final_views.values().filter_map(|v|v.snapshot.as_ref().map(|s|final_head.saturating_sub(s.as_of.number))).max().unwrap_or(u64::MAX);
    let steady_progress=steady_anchors.len()==pools&&steady_anchors.values().all(|anchors|anchors.len()>=2);
    let steady_availability=steady_available_ms.len()==pools&&steady_available_ms.iter().all(|(pool,available)|steady_unavailable_ms.get(pool).copied().unwrap_or(0)*9<=*available);
    let steady_actionability=steady_actionable_ms.len()==pools&&steady_actionable_ms.iter().all(|(pool,available)|steady_nonactionable_ms.get(pool).copied().unwrap_or(0)*9<=*available);
    let burst_ok=burst_detected_seconds.len()==pools&&burst_detected_seconds.values().all(|seconds|*seconds<=30);
    let mass_eligible=peak_eligible>=positions*9/10&&max_queue>=(manifest.queue_capacity.min(positions*9/10)).saturating_sub(1);
    let pass=model.unsupported.lock().unwrap().is_empty()&&model.native_batches.load(Ordering::Relaxed)>0&&model.external_batches.load(Ordering::Relaxed)>0&&stop_reason.is_none()&&correctness_errors.is_empty()&&max_rps<=rpc_limit&&final_verified&&final_lag<=100&&percentile(steady_lags.clone(),95)<=100&&steady_progress&&steady_availability&&steady_actionability&&burst_ok&&mass_eligible&&outage_recovered_seconds.is_some_and(|s|s<=30)&&restart_ready_ms.is_some_and(|ms|ms<=45_000)&&saw_stale.len()==pools&&saw_low.len()==pools&&saw_maturity.len()==pools&&matches!(app_result,Ok(Ok(Ok(_))))&&matches!(ingest_result,Ok(Ok(Ok(()))));
    let result=json!({"passed":pass,"kind":"synthetic_http_full_ingestion_and_dry_run_loop","evm_execution_qualified":false,"cold_discovery":cold,"pools":pools,"markets_per_pool":2,"positions":positions,"block_ms":BLOCK_MS,"rpc_limit":rpc_limit,"max_rolling_second_requests":max_rps,"response_latency_ms":std::env::var("BARTERLEND_LOAD_LATENCY_MS").unwrap_or_else(|_|"100".into()),"elapsed_ms":started.elapsed().as_millis(),"initial_ready_ms":ready_at,"restart_ready_ms":restart_ready_ms,"outage_recovery_seconds":outage_recovered_seconds,"burst_detection_seconds_per_pool":burst_detected_seconds,"phase_duration_seconds":duration,"requests":calls.len(),"methods":counts,"injected_statuses":*model.statuses.lock().unwrap(),"snapshot_lag_blocks_p95":percentile(lags.clone(),95),"steady_lag_blocks_p95":percentile(steady_lags.clone(),95),"steady_lag_blocks_p99":percentile(steady_lags,99),"snapshot_lag_blocks_max":lags.iter().max(),"final_lag_blocks":final_lag,"max_queue_len":max_queue,"cpu_seconds_including_harness":cpu_end-cpu_start,"process_peak_rss_bytes_including_harness":rss,"oracle_low_pools":saw_low.len(),"oracle_stale_pools":saw_stale.len(),"maturity_pools":saw_maturity.len(),"correctness_errors":correctness_errors,"stop_reason":stop_reason,"samples":samples});
    let mut result=result;result["final_all_pools_verified"]=json!(final_verified);result["unavailable_pool_milliseconds_after_ready"]=json!(unavailable_pool_ms);result["peak_execution_ready_positions"]=json!(peak_eligible);result["build_profile"]=json!(if cfg!(debug_assertions){"debug"}else{"release"});
    result["steady_available_ms_per_pool"]=json!(steady_available_ms);result["steady_unavailable_ms_per_pool"]=json!(steady_unavailable_ms);result["steady_distinct_anchors_per_pool"]=json!(steady_anchors.iter().map(|(p,a)|(p.to_string(),a.len())).collect::<BTreeMap<_,_>>());
    result["steady_progress_passed"]=json!(steady_progress);result["steady_availability_passed"]=json!(steady_availability);result["burst_slo_passed"]=json!(burst_ok);result["mass_eligibility_passed"]=json!(mass_eligible);result["oracle_rpc_unavailable_snapshots"]=json!(oracle_rpc_unavailable);result["actual_phase_duration_seconds"]=json!(model.seconds(final_head));
    result["lending_burst_events"]=json!(6*pools);result["lending_burst_changed_positions"]=json!(4*pools);result["constant_lending_churn_qualified"]=json!(true);result["candidate_queue_age_measured"]=json!(false);
    result["churn"]=json!("one rotating collateral change per two blocks per pool, excluding deliberately thin-collateral sentinel");
    result["steady_actionable_ms_per_pool"]=json!(steady_actionable_ms);result["steady_nonactionable_ms_per_pool"]=json!(steady_nonactionable_ms);result["steady_actionability_passed"]=json!(steady_actionability);result["steady_phase_start_seconds"]=json!(steady_start);result["rpc_calls"]=json!(*model.call_details.lock().unwrap());
    result["restart_verified_ms"]=json!(restart_verified_ms);result["restart_actionable_ms"]=json!(restart_ready_ms);
    result["rpc_limit"]=json!(rpc_limit);
    result["rate_observation"]=json!("complete_http_request_read_before_artificial_latency");
    result["max_rolling_second_processing_starts"]=json!(max_processing_rps);result["max_rolling_second_tcp_accepts"]=json!(max_tcp_accept_rps);
    result["positions_per_market"]=json!(positions/(pools*2));
    result["engine"]=json!(if manifest.sync.current_state.is_some(){"current_debt"}else{"historical"});
    result["read_optimizations"]=json!(manifest.rpc.read_optimizations);
    result["native_batches"]=json!(model.native_batches.load(Ordering::Relaxed));
    result["external_batches"]=json!(model.external_batches.load(Ordering::Relaxed));
    result["unsupported_calls"]=json!(*model.unsupported.lock().unwrap());
    write_report(&result);assert!(pass,"local load acceptance failed; inspect LOAD_RESULT report");
}
