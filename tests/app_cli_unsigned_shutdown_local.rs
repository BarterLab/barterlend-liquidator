//! Actual binary SIGINT while unsigned RPC is held: no reserve/sign/send, and
//! stopped telemetry exits before the proxy releases its unreadable response.
#![cfg(unix)]
#![allow(dead_code,unused_imports)]
include!("support/app_local_fixture.rs");

#[derive(Clone,Copy,Debug)]
enum Hold { Quoter, Estimate }

#[derive(Default)]
struct HeldStats { held: usize, sends: usize, requests: usize }
struct HeldProxy { url: String, stats: Arc<Mutex<HeldStats>>, release: Arc<AtomicBool>, stop: Arc<AtomicBool>, thread: Option<thread::JoinHandle<()>> }
impl HeldProxy {
    fn new(origin: reqwest::Url, quoter: Address, hold: Hold) -> Self {
        validate_loopback_url(origin.as_str()).unwrap();
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
        let url=format!("http://{}",listener.local_addr().unwrap());
        let stats=Arc::new(Mutex::new(HeldStats::default()));let counters=stats.clone();
        let release=Arc::new(AtomicBool::new(false));let released=release.clone();
        let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();
        let runtime=tokio::runtime::Handle::current();
        let client=reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let thread=thread::spawn(move || {
            let mut connections=Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let (mut stream,_)=match listener.accept() {
                    Ok(connection)=>connection,
                    Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>{thread::sleep(Duration::from_millis(1));continue;},
                    Err(error)=>panic!("held proxy listener: {error}"),
                };
                if stopping.load(Ordering::Acquire) { break; }
                // macOS may inherit O_NONBLOCK from the listening socket.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                let stats=counters.clone();let release=released.clone();let client=client.clone();let origin=origin.clone();let runtime=runtime.clone();
                connections.push(thread::spawn(move || {
                    let Some(request)=read_request(&stream) else { return; };
                    let selected=match hold {
                        // Only preparation quotes; startup factory()/WETH9() must complete.
                        Hold::Quoter=>request["method"]=="eth_call" && serde_json::from_value::<Address>(request["params"][0]["to"].clone()).ok()==Some(quoter)
                            && request["params"][0]["data"].as_str().is_some_and(|data|data.starts_with("0xcdca1753")),
                        Hold::Estimate=>request["method"]=="eth_estimateGas",
                    };
                    {let mut counters=stats.lock().unwrap();counters.requests+=1;if selected {counters.held+=1;}
            if request["method"]=="eth_sendRawTransaction" {counters.sends+=1;}}
                    if selected { while !release.load(Ordering::Acquire) {thread::sleep(Duration::from_millis(5));} return; }
                    let response=runtime.block_on(async {client.post(origin).json(&request).send().await?.error_for_status()?.json::<Value>().await}).expect("healthy held proxy must forward actual RPC");
                    let body=serde_json::to_vec(&response).unwrap();
                    if write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).is_err() {return;}
                    let _=stream.write_all(&body);
                }));
            }
            for connection in connections {connection.join().unwrap();}
        });
        Self {url,stats,release,stop,thread:Some(thread)}
    }
}
impl Drop for HeldProxy {
    fn drop(&mut self) {self.release.store(true,Ordering::Release);self.stop.store(true,Ordering::Release);let _=TcpStream::connect(self.url.strip_prefix("http://").unwrap());if let Some(thread)=self.thread.take() {if thread.join().is_err() && !thread::panicking() {panic!("held proxy worker failed");}}}
}

struct UnsignedCli { child: Option<std::process::Child>, output: std::path::PathBuf }
impl UnsignedCli {
    fn start(manifest: &Path, output: &Path, live: bool) -> Self {
        let log=std::fs::File::create(output).unwrap();
        let mut command=std::process::Command::new(env!("CARGO_BIN_EXE_barterlend-liquidator"));
        command.arg("--manifest").arg(manifest).arg("--cycles").arg("0").env_remove("BL_LIQUIDATOR_PRIVATE_KEY");
        if live {command.arg("--local-live").env("BL_LIQUIDATOR_PRIVATE_KEY",LOCAL_DEV_KEY);}
        let child=command.stdout(std::process::Stdio::from(log.try_clone().unwrap())).stderr(std::process::Stdio::from(log)).spawn().unwrap();
        Self {child:Some(child),output:output.to_path_buf()}
    }
    async fn interrupt_and_finish(&mut self) {
        let pid=self.child.as_ref().unwrap().id();
        assert!(std::process::Command::new("/bin/kill").arg("-TERM").arg(pid.to_string()).status().unwrap().success());
        let deadline=Instant::now()+Duration::from_secs(10);
        loop {
            if let Some(status)=self.child.as_mut().unwrap().try_wait().unwrap() {self.child.take();assert!(status.success(),"CLI failed: {}",std::fs::read_to_string(&self.output).unwrap());break;}
            assert!(Instant::now()<deadline,"unsigned CLI did not exit before held RPC was released: {}",std::fs::read_to_string(&self.output).unwrap());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
impl Drop for UnsignedCli {fn drop(&mut self) {if let Some(mut child)=self.child.take() {let _=child.kill();let _=child.wait();}}}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore="requires its own fresh isolated seeded Monad-mode Anvil"]
async fn actual_cli_sigint_cancels_held_quoter_and_estimate_before_any_signature() {
    let origin=validate_loopback_url(&std::env::var("BARTERLEND_LOCAL_RPC_URL").expect("explicit local origin required")).unwrap();
    let seed:Seed=serde_json::from_slice(&std::fs::read(std::env::var("BARTERLEND_LOCAL_MANIFEST").expect("local seed required")).unwrap()).unwrap();
    let mut local=LocalTx {client:reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().unwrap(),url:origin.clone(),timestamp:2};
    assert_eq!(local.rpc("eth_chainId",json!([])).await,"0x7a69");assert_eq!(local.rpc("eth_blockNumber",json!([])).await,"0x0","exclusive fresh node required");
    let reader=Arc::new(HttpReader::new(origin.as_str(),RpcConfig {min_request_interval:Duration::from_millis(1),..Default::default()}).unwrap());
    let chain=reader.identity().await.unwrap();assert_eq!(chain.genesis_hash,seed.genesis_hash);
    let market:u64=seed.market0.parse().unwrap();
    local.send(seed.borrower_a,seed.collateral,TestToken::mintCall {amount:U256::from(400*WAD)}).await;
    local.send(seed.borrower_a,seed.collateral,TestToken::approveCall {spender:seed.pool,amount:U256::MAX}).await;
    local.send(seed.borrower_a,seed.pool,CreditPool::depositCollateralCall {account:seed.borrower_a,market,amount:400*WAD}).await;
    local.send(seed.borrower_a,seed.pool,CreditPool::placeBorrowOrderCall {account:seed.borrower_a,market,APR:U256::from(1000),amount:100*WAD}).await;
    local.send(seed.provider_a,seed.asset,TestToken::mintCall {amount:U256::from(100*WAD)}).await;
    local.send(seed.provider_a,seed.asset,TestToken::approveCall {spender:seed.pool,amount:U256::MAX}).await;
    local.send(seed.provider_a,seed.pool,CreditPool::depositCall {recipient:seed.provider_a,amount:100*WAD}).await;
    local.send(seed.provider_a,seed.pool,CreditPool::provideExactInCall {account:seed.provider_a,market,assetsAmount:100*WAD,minLoanAmount:0,maxCrossedOrders:10}).await;
    local.send(seed.borrower_a,seed.oracle,TestOracle::setPriceCall {newPrice:U256::from(WAD/2)}).await;
    let at=reader.head(Commitment::Latest).await.unwrap();let oracle_hash=keccak256(reader.get_code(seed.oracle,&at).await.unwrap());
    let balance=local.rpc("eth_getBalance",json!([seed.operator,"latest"])).await;
    for live in [false,true] {
        for hold in [Hold::Quoter,Hold::Estimate] {
            let proxy=HeldProxy::new(origin.clone(),seed.quoter,hold);let directory=tempfile::tempdir().unwrap();
            let manifest=manifest(&seed,proxy.url.clone(),chain,oracle_hash,directory.path());let path=directory.path().join("manifest.json");
            std::fs::write(&path,serde_json::to_vec(&manifest).unwrap()).unwrap();
            let mut child=UnsignedCli::start(&path,&directory.path().join("cli.log"),live);
            let held_deadline=Instant::now()+Duration::from_secs(30);
            loop {
                if proxy.stats.lock().unwrap().held>0 { break; }
                if let Some(status)=child.child.as_mut().unwrap().try_wait().unwrap() {
                    panic!("CLI exited before unsigned request live={live} hold={hold:?} status={status}: {}",std::fs::read_to_string(&child.output).unwrap());
                }
                if Instant::now()>=held_deadline {
                    let stats=proxy.stats.lock().unwrap();
                    panic!("unsigned request timeout live={live} hold={hold:?} held={} sends={} requests={}: {}",stats.held,stats.sends,stats.requests,std::fs::read_to_string(&child.output).unwrap());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let before=Instant::now();child.interrupt_and_finish().await;
            assert!(!proxy.release.load(Ordering::Acquire),"RPC response was released before CLI exit");assert_eq!(proxy.stats.lock().unwrap().sends,0);
            assert!(!directory.path().join("live").exists());
            assert!(!directory.path().join("dry-run").exists());
            let output=std::fs::read_to_string(&child.output).unwrap();
            let records:Vec<Value>=output.lines().filter_map(|line|line.split_once(" details="))
                .filter_map(|(_,details)|serde_json::from_str(details).ok()).collect();
            assert!(records.iter().any(|r|r["event"]=="stopped"),"missing stopped event: {output}");
            assert_eq!(local.rpc("eth_getTransactionCount",json!([seed.operator,"latest"])).await,"0x0");assert_eq!(local.rpc("eth_getTransactionCount",json!([seed.operator,"pending"])).await,"0x0");
            assert_eq!(local.rpc("eth_getBalance",json!([seed.operator,"latest"])).await,balance);
            println!("PASS actual SIGINT live={live} hold={hold:?}: stopped in {:?}, no signature/send/nonce/balance change",before.elapsed());
        }
    }
}
