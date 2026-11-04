//! Keyless config generation against a strict loopback JSON-RPC fixture.
use super::*;
use alloy::{primitives::Bytes, sol_types::SolValue};
use serde_json::{json, Value};
use std::{io::{BufRead, BufReader}, net::TcpListener,
    sync::{Mutex, atomic::{AtomicBool,Ordering}}, thread, time::Duration};

const EXECUTOR: Address = Address::repeat_byte(41);
const OWNER: Address = Address::repeat_byte(42);
const OPERATOR: Address = Address::repeat_byte(43);
const LENDING: Address = Address::repeat_byte(44);
const CODE: &[u8] = &[0x60,0x00,0x56];

#[derive(Default)]
struct Faults {
    network: Option<barterlend_state::ChainIdentity>,
    bad_code: Option<Address>, no_manager: bool, no_kyber: bool, bad_kyber_pin: bool,
    bad_v4_quoter_binding: bool, rpc_failure: bool,
    denied_tokens: BTreeSet<Address>, denied_pools: BTreeSet<B256>, no_intermediates: bool,
}
fn encoded(value: impl SolValue) -> Value { json!(Bytes::from(value.abi_encode())) }
fn selector(signature: &str) -> Vec<u8> { keccak256(signature.as_bytes()).as_slice()[..4].to_vec() }
struct Stub { url:String, stop:Arc<AtomicBool>, task:Option<thread::JoinHandle<()>>, methods:Arc<Mutex<Vec<String>>> }
impl Drop for Stub {
    fn drop(&mut self) { self.stop.store(true,Ordering::Release); let result=self.task.take().unwrap().join(); if !thread::panicking() {result.unwrap();} }
}
impl Stub {
    fn new(preset:monad::Preset,faults:Faults) -> Self {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();
        let methods=Arc::new(Mutex::new(Vec::new()));let recorded=methods.clone();
        let task=thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream,_)=match listener.accept() {
                    Ok(value)=>value,
                    Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{thread::sleep(Duration::from_millis(1));continue;},
                    Err(e)=>panic!("loopback accept: {e}"),
                };
                stream.set_nonblocking(false).unwrap();stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut input=BufReader::new(stream.try_clone().unwrap());let mut length=0;
                loop {
                    let mut line=String::new();if input.read_line(&mut line).unwrap()==0 || line=="\r\n" {break;}
                    if let Some((k,v))=line.split_once(':') {if k.eq_ignore_ascii_case("content-length") {length=v.trim().parse::<usize>().unwrap();}}
                }
                if length==0 {continue;}assert!(length<1_048_576);
                let mut body=vec![0;length];input.read_exact(&mut body).unwrap();
                let request:Value=serde_json::from_slice(&body).unwrap();
                let reply=|request:&Value| {
                    let method=request["method"].as_str().unwrap();recorded.lock().unwrap().push(method.into());
                    assert!(matches!(method,"eth_chainId"|"eth_getBlockByNumber"|"eth_getCode"|"eth_call"|"eth_getTransactionCount"|"eth_gasPrice"),"unexpected mutation/API method: {method}");
                    if faults.rpc_failure && method=="eth_getCode" {return json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"fixture unavailable"}});}
                    let chain=faults.network.unwrap_or(preset.chain);
                    let result=match method {
                        "eth_chainId"=>json!(format!("0x{:x}",chain.chain_id)),
                        "eth_gasPrice"=>json!("0x174876e800"),
                        "eth_getTransactionCount"=>json!("0x1"),
                        "eth_getBlockByNumber"=>if request["params"][0]=="0x0" {
                            json!({"number":"0x0","hash":chain.genesis_hash,"parentHash":B256::ZERO,"timestamp":"0x0"})
                        } else {json!({"number":"0x2a","hash":B256::repeat_byte(1),"parentHash":B256::repeat_byte(2),"timestamp":"0x64"})},
                        "eth_getCode"=>{
                            let address:Address=serde_json::from_value(request["params"][0].clone()).unwrap();
                            json!(Bytes::copy_from_slice(if faults.bad_code==Some(address) {&[0x01]} else {CODE}))
                        },
                        "eth_call"=>{
                            let tx=&request["params"][0];let to:Address=serde_json::from_value(tx["to"].clone()).unwrap();
                            let data:Bytes=serde_json::from_value(tx.get("data").or_else(||tx.get("input")).unwrap().clone()).unwrap();
                            let sig=&data[..4];
                            if sig==selector("owner()") {encoded(OWNER)}
                            else if sig==selector("operators(address)") {encoded(true)}
                            else if sig==selector("router()") {encoded(preset.v3.router)}
                            else if sig==selector("factory()") {encoded(preset.v3.factory)}
                            else if sig==selector("WETH9()") {encoded(preset.v3.wrapped_native)}
                            else if sig==selector("decimals()") {encoded(U256::from(18))}
                            else if sig==selector("lendingFactories()") {encoded(vec![LENDING])}
                            else if sig==selector("lendingFactoryCodeHashes(address)") {encoded(keccak256(CODE))}
                            else if sig==selector("poolVersion()") {encoded(U256::from(5))}
                            else if sig==selector("previousFactory()") {encoded(Address::ZERO)}
                            else if sig==selector("feeAmountTickSpacing(uint24)") {encoded(U256::from(10))}
                            else if sig==selector("allowedFees(uint24)") {encoded(true)}
                            else if sig==selector("verifiedTokens(address)") {encoded(!faults.denied_tokens.contains(&Address::from_slice(&data[16..36])))}
                            else if sig==selector("intermediates(address)") {encoded(!faults.no_intermediates)}
                            else if sig==selector("allowedV4Pools(bytes32)") {encoded(!faults.denied_pools.contains(&B256::from_slice(&data[4..36])))}
                            else if sig==selector("v4Manager()") {encoded(if faults.no_manager {Address::ZERO} else {preset.v4.manager})}
                            else if sig==selector("poolManager()") {encoded(if faults.bad_v4_quoter_binding {Address::ZERO} else {preset.v4.manager})}
                            else if sig==selector("kyberRouter()") {encoded(if faults.no_kyber {Address::ZERO} else {preset.kyber.router})}
                            else if sig==selector("kyberRouterCodeHash()") {encoded(if faults.bad_kyber_pin {B256::ZERO} else {preset.kyber.router_code_hash})}
                            else {panic!("unhandled fixture getter at {to}: {}",Bytes::copy_from_slice(sig))}
                        },
                        _=>unreachable!(),
                    };
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                };
                let response=if let Some(batch)=request.as_array() {Value::Array(batch.iter().map(reply).collect())} else {reply(&request)};
                let body=serde_json::to_vec(&response).unwrap();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {url,stop,task:Some(task),methods}
    }
}
fn fixture_preset() -> monad::Preset {
    let mut p=monad::Preset::load().unwrap();let hash=keccak256(CODE);
    p.v3_code_hashes=V3CodeHashes {factory:hash,router:hash,quoter:hash,wrapped_native:hash};
    p.v4.manager_code_hash=hash;p.v4.quoter_code_hash=hash;p.kyber.router_code_hash=hash;p
}
async fn generate(preset:monad::Preset,faults:Faults,intermediates:Vec<Address>) -> (Result<LocalManifest>,Stub) {
    let server=Stub::new(preset.clone(),faults);
    let reader=Arc::new(HttpReader::new(&server.url,barterlend_state::rpc::RpcConfig {
        min_request_interval:Duration::ZERO,max_retries:0,request_timeout:Duration::from_secs(2),..Default::default() }).unwrap());
    // Production discover() still requires HTTPS for Mainnet. Only this private helper
    // gets a loopback transport; no insecure CLI flag is introduced.
    let input=Init {profile:"monad".into(),rpc_url:"https://example.invalid".into(),executor:EXECUTOR,
        operator:OPERATOR,pools:vec![],lending_factories:vec![],quoter:None,intermediates};
    use crate::tx::rpc::TxRpc;
    let gas=crate::tx::rpc::LocalTxRpc::new(&server.url,reader.clone()).unwrap().gas_price().await.unwrap();
    let result=tokio::time::timeout(Duration::from_secs(10),discover_with_reader(input,reader,Some(preset),gas)).await.unwrap();
    (result,server)
}
#[tokio::test]
async fn mainnet_init_fills_all_sources_and_roundtrips_external_toml() {
    let p=fixture_preset();let expected_quoter=p.v3.quoter;
    let (result,server)=generate(p,Faults::default(),vec![]).await;let config=result.unwrap();
    assert_eq!(config.v3.quoter,expected_quoter);assert_eq!(config.v4.as_ref().unwrap().pools.len(),3);
    assert_eq!(config.kyber.as_ref().unwrap().requests_per_second,3);assert!(config.kyber.as_ref().unwrap().api_key_env.is_none());
    assert!(config.discovery.is_some());assert_eq!(config.v3.intermediates.len(),3);
    assert_eq!(config.fees.max_gas_limit,4_000_000);assert_eq!(config.additional_profit_asset,U256::ZERO);
    assert_eq!(config.fees.max_fee_native,config.fees.max_gas_price*U256::from(4_000_000));
    assert_eq!(config.max_total_native_spend,config.fees.max_fee_native*U256::from(10)+config.fees.native_reserve);
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.toml");save_new(&path,&config).unwrap();
    let loaded=load(&path,None).unwrap();assert_eq!(loaded.v4,config.v4);assert_eq!(loaded.kyber.as_ref().unwrap().router,config.kyber.as_ref().unwrap().router);
    let before=std::fs::read(&path).unwrap();assert!(save_new(&path,&config).is_err());assert_eq!(std::fs::read(&path).unwrap(),before);
    assert!(server.methods.lock().unwrap().len()<200);
}
#[tokio::test]
async fn mainnet_init_keeps_token_and_pool_permissions_unchanged() {
    let p=fixture_preset();let usdc=p.intermediates[0];let btc=p.v4.pools[2].currency1;
    let denied=p.v4.pools[0].id().unwrap();
    let faults=Faults {denied_tokens:BTreeSet::from([btc]),denied_pools:BTreeSet::from([denied]),no_intermediates:true,..Default::default()};
    let (result,_server)=generate(p,faults,vec![]).await;let config=result.unwrap();
    assert!(config.v3.intermediates.is_empty());assert!(!config.v3.allowed_tokens.contains(&btc));
    assert_eq!(config.v4.unwrap().pools.len(),1);
    let (result,_server)=generate(fixture_preset(),Faults {no_intermediates:true,..Default::default()},vec![usdc]).await;
    assert_eq!(result.unwrap_err().to_string(),"monad_config_intermediate_not_approved");
}
#[tokio::test]
async fn missing_v4_or_kyber_permissions_do_not_silently_disable_sources() {
    for (faults,expected) in [
        (Faults {no_manager:true,..Default::default()},"monad_config_v4_manager_not_configured"),
        (Faults {no_kyber:true,..Default::default()},"monad_config_kyber_not_configured"),
        (Faults {bad_kyber_pin:true,..Default::default()},"monad_config_kyber_not_configured"),
        (Faults {denied_pools:fixture_preset().v4.pools.iter().map(|p|p.id().unwrap()).collect(),..Default::default()},"monad_config_no_approved_v4_pools"),
        (Faults {denied_tokens:BTreeSet::from([fixture_preset().intermediates[0]]),..Default::default()},"monad_config_no_approved_v4_pools")] {
        let (result,_server)=generate(fixture_preset(),faults,vec![]).await;assert_eq!(result.unwrap_err().to_string(),expected);
    }
}
#[tokio::test]
async fn wrong_network_code_or_binding_and_rpc_failure_reject_the_configuration() {
    let p=fixture_preset();
    for (faults,expected) in [
        (Faults {network:Some(barterlend_state::ChainIdentity {chain_id:10143,..p.chain}),..Default::default()},"monad_config_network_mismatch"),
        (Faults {network:Some(barterlend_state::ChainIdentity {genesis_hash:B256::repeat_byte(99),..p.chain}),..Default::default()},"monad_config_network_mismatch"),
        (Faults {bad_code:Some(p.v3.router),..Default::default()},"monad_config_v3_deployment_mismatch"),
        (Faults {bad_code:Some(p.v3.quoter),..Default::default()},"monad_config_v3_deployment_mismatch"),
        (Faults {bad_code:Some(p.v4.manager),..Default::default()},"V4 manager/quoter code or binding differs"),
        (Faults {bad_code:Some(p.v4.quoter),..Default::default()},"V4 manager/quoter code or binding differs"),
        (Faults {bad_code:Some(p.kyber.router),..Default::default()},"monad_config_kyber_code_mismatch"),
        (Faults {bad_v4_quoter_binding:true,..Default::default()},"V4 manager/quoter code or binding differs"),
        (Faults {rpc_failure:true,..Default::default()},"fixture unavailable")] {
        let (result,_server)=generate(p.clone(),faults,vec![]).await;
        let error=result.unwrap_err().to_string();assert!(error.contains(expected),"expected {expected}, got {error}");
    }
}

#[tokio::test]
async fn network_policy_and_non_mainnet_quoter_requirement_remain_strict() {
    let input=Init {profile:"local".into(),rpc_url:"http://127.0.0.1:9".into(),executor:EXECUTOR,
        operator:OPERATOR,pools:vec![],lending_factories:vec![],quoter:None,intermediates:vec![]};
    assert_eq!(discover(input.clone()).await.unwrap_err().to_string(),"config init requires --quoter outside Monad Mainnet");
    let mut input=input;input.profile="monad".into();assert!(discover(input).await.is_err());
}
#[test]
fn bundled_mainnet_deployments_match_existing_reviewed_fork_fixture() {
    let fixture:Value=serde_json::from_str(include_str!("../fixtures/monad-v4.json")).unwrap();
    let p=monad::Preset::load().unwrap();assert_eq!(p.chain.chain_id,143);
    assert_eq!(json!(p.v3.quoter),fixture["v3_contracts"]["quoter"]["address"]);
    assert_eq!(json!(p.v3_code_hashes.quoter),fixture["v3_contracts"]["quoter"]["code_hash"]);
    assert_eq!(json!(p.v4.manager),fixture["v4"]["manager"]);assert_eq!(json!(p.v4.manager_code_hash),fixture["v4"]["manager_code_hash"]);
    assert_eq!(json!(p.v4.quoter_code_hash),fixture["v4"]["quoter_code_hash"]);
    assert_eq!(p.v4.pools.len(),3);for key in &p.v4.pools {key.id().unwrap();}
    p.kyber.validate(p.chain).unwrap();
}

#[tokio::test]
async fn local_generation_keeps_v3_only_and_two_million_gas_default() {
    let mut p=fixture_preset();p.chain.chain_id=31337;let quoter=p.v3.quoter;
    let server=Stub::new(p,Faults::default());
    let input=Init {profile:"local".into(),rpc_url:server.url.clone(),executor:EXECUTOR,
        operator:OPERATOR,pools:vec![],lending_factories:vec![],quoter:Some(quoter),intermediates:vec![]};
    let config=tokio::time::timeout(Duration::from_secs(20),discover(input)).await.unwrap().unwrap();
    assert_eq!(config.fees.max_gas_limit,2_000_000);assert_eq!(config.v3.quoter,quoter);
    assert!(config.v4.is_none() && config.kyber.is_none());
    assert!(server.methods.lock().unwrap().iter().any(|method|method=="eth_gasPrice"));
}
