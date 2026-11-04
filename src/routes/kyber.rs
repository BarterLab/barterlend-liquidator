//! ERC20-only Kyber APIv1 integration. API responses are untrusted input.
use std::{sync::Arc, time::{Duration, Instant}};
use alloy::{primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256}, sol, sol_types::{SolCall, SolValue}};
use barterlend_state::{ChainIdentity, HttpReader, StateReader};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, MutexGuard};
use super::*;

sol! {
    struct Description {
        address srcToken; address dstToken; address[] srcReceivers; uint256[] srcAmounts;
        address[] feeReceivers; uint256[] feeAmounts; address dstReceiver;
        uint256 amount; uint256 minReturnAmount; uint256 flags; bytes permit;
    }
    struct Execution { address callTarget; address approveTarget; bytes targetData; Description desc; bytes clientData; }
    function swap(Execution execution) external payable returns (uint256 returnAmount, uint256 gasUsed);
}
const MAGIC: FixedBytes<4> = FixedBytes::new(*b"KYBR");
const MAX_RESPONSE: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KyberConfig {
    pub base_url: String,
    pub router: Address,
    pub router_code_hash: B256,
    pub client_id: String,
    /// Name of an environment variable, never the API secret itself.
    #[serde(default)] pub api_key_env: Option<String>,
    #[serde(default = "default_rps")] pub requests_per_second: u32,
    #[serde(default = "default_ttl")] pub ttl_seconds: u64,
}
fn default_rps() -> u32 { 3 }
fn default_ttl() -> u64 { 30 }
fn invalid() -> RouteError { RouteError::Identity("Kyber response or calldata does not match the request".into()) }
fn unavailable() -> RouteError { RouteError::Unavailable("Kyber API unavailable".into()) }

impl KyberConfig {
    pub fn validate(&self, chain: ChainIdentity) -> RouteResult<()> {
        let url = reqwest::Url::parse(&self.base_url).map_err(|_| invalid())?;
        let production = chain.chain_id == 143 && url.scheme() == "https"
            && matches!(url.host_str(), Some("aggregator-api.kyberswap.com" | "api.kyberswap.com"));
        let local = chain.chain_id == 31337 && crate::tx::rpc::validate_loopback_url(&self.base_url).is_ok();
        if !(production || local) || !url.username().is_empty() || url.password().is_some() || url.query().is_some()
            || url.fragment().is_some() || self.router.is_zero() || self.router_code_hash.is_zero()
            || !(1..=3).contains(&self.requests_per_second) || !(1..=30).contains(&self.ttl_seconds)
            || self.client_id.is_empty() || self.client_id.len() > 128 || !self.client_id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)) {
            return Err(RouteError::InvalidConfig("invalid Kyber network, endpoint, identity or bounded API policy".into()));
        }
        Ok(())
    }
}

pub struct KyberProvider {
    reader: Arc<HttpReader>, chain: ChainIdentity, config: KyberConfig,
    client: reqwest::Client, next_request: Mutex<Instant>, slippage_bps: u32, max_age_blocks: u64,
}

// Keep one API attempt in flight and leave a full interval after its completion.
// Dropping a cancelled request also preserves the interval; waiting consumes none.
struct ApiRequestPermit<'a> { next: MutexGuard<'a, Instant>, interval: Duration }
impl Drop for ApiRequestPermit<'_> {
    fn drop(&mut self) { *self.next = Instant::now() + self.interval; }
}
impl KyberProvider {
    pub fn new(reader: Arc<HttpReader>, chain: ChainIdentity, config: KyberConfig, limits: &RouteLimits) -> RouteResult<Self> {
        config.validate(chain)?; limits.validate()?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-client-id", config.client_id.parse().map_err(|_| invalid())?);
        if let Some(name) = &config.api_key_env {
            let secret = zeroize::Zeroizing::new(std::env::var(name).map_err(|_| RouteError::InvalidConfig("Kyber API key environment is missing".into()))?);
            let mut value = reqwest::header::HeaderValue::from_str(&secret).map_err(|_| invalid())?;
            value.set_sensitive(true); headers.insert("X-Api-Key", value);
        }
        let client = reqwest::Client::builder().retry(reqwest::retry::never()).no_proxy().user_agent("barterlend-liquidator/0.1").default_headers(headers).redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3)).build().map_err(|_| unavailable())?;
        Ok(Self { reader, chain, config, client, next_request: Mutex::new(Instant::now()),
            slippage_bps: limits.slippage_bps, max_age_blocks: limits.max_age_blocks })
    }
    async fn request(&self, request: reqwest::RequestBuilder) -> RouteResult<Value> {
        let request = request.build().map_err(|_| unavailable())?;
        for attempt in 0..=1 {
            let next = self.next_request.lock().await;
            tokio::time::sleep_until((*next).into()).await;
            let permit = ApiRequestPermit { next,
                interval: Duration::from_nanos(1_000_000_000u64.div_ceil(u64::from(self.config.requests_per_second))) };
            let mut response = self.client.execute(request.try_clone().ok_or_else(unavailable)?).await.map_err(|_| unavailable())?;
            if (response.status().as_u16() == 429 || response.status().is_server_error()) && attempt == 0 {
                let delay = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).min(2);
                drop(response); drop(permit);
                tokio::time::sleep(Duration::from_secs(delay)).await; continue;
            }
            if !response.status().is_success() || response.content_length().is_some_and(|v| v > MAX_RESPONSE as u64) { return Err(unavailable()); }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
                if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE { return Err(unavailable()); }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
            if value.get("code").and_then(Value::as_i64) != Some(0) { return Err(unavailable()); }
            return value.get("data").cloned().ok_or_else(unavailable);
        }
        Err(unavailable())
    }
    pub async fn quote(&self, request: &ExactInputRequest) -> RouteResult<RouteQuote> {
        if request.token_in == request.token_out || request.token_in.is_zero() || request.token_out.is_zero()
            || request.amount_in.is_zero() || request.executor.is_zero() { return Err(invalid()); }
        if self.reader.identity().await? != self.chain { return Err(invalid()); }
        let code = self.reader.get_code(self.config.router, &request.at).await?;
        if code.is_empty() || keccak256(code) != self.config.router_code_hash { return Err(invalid()); }
        let started = Instant::now();
        let base = self.config.base_url.trim_end_matches('/');
        let route = self.request(self.client.get(format!("{base}/monad/api/v1/routes")).query(&[
            ("tokenIn", request.token_in.to_string()), ("tokenOut", request.token_out.to_string()),
            ("amountIn", request.amount_in.to_string()), ("gasInclude", "false".into()),
            // Exclude RFQ sources. Other routes may use AMMs or on-chain orderbooks;
            // RFQ reservations require a separate execution policy.
            ("excludeRFQSources", "true".into()),
        ])).await?;
        let summary = route.get("routeSummary").ok_or_else(invalid)?;
        if address(&route["routerAddress"])? != self.config.router || address(&summary["tokenIn"])? != request.token_in
            || address(&summary["tokenOut"])? != request.token_out || number(&summary["amountIn"])? != request.amount_in
            || number(&summary["amountOut"])?.is_zero() { return Err(invalid()); }
        let built = self.request(self.client.post(format!("{base}/monad/api/v1/route/build")).json(&json!({
            "routeSummary": summary, "sender": request.executor, "recipient": request.executor,
            "slippageTolerance": self.slippage_bps, "enableGasEstimation": false,
        }))).await?;
        if started.elapsed() >= Duration::from_secs(self.config.ttl_seconds) { return Err(RouteError::Stale("Kyber quote exceeded API TTL".into())); }
        if number(&built["amountOut"])?.checked_add(U256::ONE).is_none_or(|v| v < number(&summary["amountOut"]).unwrap_or(U256::MAX)) { return Err(invalid()); }
        let (data, output, minimum) = validate_build(&built, request, self.config.router, self.slippage_bps)?;
        let path = (MAGIC, self.config.router, data).abi_encode().into();
        let mut quote = RouteQuote { token_in: request.token_in, token_out: request.token_out, amount_in: request.amount_in,
            amount_out: output, minimum_amount_out: minimum, executor: request.executor, router: Some(self.config.router),
            route: RouteKind::Kyber { path },
            // This is the liquidation simulation anchor, not a claim that the API quoted historical state.
            validity: QuoteValidity::at(request.at, self.config.ttl_seconds, self.max_age_blocks)?,
            quoter_gas_estimate: U256::ZERO, search: SearchQuality { candidates_examined: 1, quote_calls: 2, budget_exhausted: false }, fingerprint: B256::ZERO };
        quote.fingerprint = quote.compute_fingerprint();
        quote.validate(request.token_in, request.token_out, request.amount_in, request.executor, &request.at)?;
        self.reader.ensure_canonical(&request.at).await?;
        Ok(quote)
    }
}

fn address(v: &Value) -> RouteResult<Address> { v.as_str().ok_or_else(invalid)?.parse().map_err(|_| invalid()) }
fn number(v: &Value) -> RouteResult<U256> {
    let text = v.as_str().ok_or_else(invalid)?;
    if text.starts_with("0x") { U256::from_str_radix(&text[2..], 16) } else { U256::from_str_radix(text, 10) }.map_err(|_| invalid())
}
fn minimum_output(output: U256, bps: u32) -> RouteResult<U256> {
    if bps >= 10_000 { return Err(invalid()); }
    let result = alloy::primitives::U512::from(output) * alloy::primitives::U512::from(10_000-bps) / alloy::primitives::U512::from(10_000);
    let limbs = result.as_limbs();
    if limbs[4..].iter().any(|n| *n != 0) { return Err(invalid()); }
    Ok(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}
pub fn validate_build(built: &Value, request: &ExactInputRequest, router: Address, slippage_bps: u32) -> RouteResult<(Bytes, U256, U256)> {
    let output = number(&built["amountOut"])?;
    if address(&built["routerAddress"])? != router || number(&built["amountIn"])? != request.amount_in || output.is_zero() { return Err(invalid()); }
    for field in ["transactionValue", "value"] {
        if let Some(value) = built.get(field) { if number(value)? != U256::ZERO { return Err(invalid()); } }
    }
    let data: Bytes = built["data"].as_str().ok_or_else(invalid)?.parse().map_err(|_| invalid())?;
    let minimum = validate_call(&data, request, minimum_output(output, slippage_bps)?)?;
    if minimum > output { return Err(invalid()); }
    Ok((data, output, minimum))
}

pub fn validate_path(path: &Bytes, request: &ExactInputRequest, router: Address, minimum: U256) -> RouteResult<()> {
    if path.len() > 65_536 { return Err(invalid()); }
    let (magic, target, data) = <(FixedBytes<4>, Address, Bytes)>::abi_decode(path).map_err(|_| invalid())?;
    if magic != MAGIC || target != router || (magic, target, data.clone()).abi_encode() != path.as_ref() { return Err(invalid()); }
    validate_call(&data, request, minimum)?; Ok(())
}

fn validate_call(data: &Bytes, request: &ExactInputRequest, minimum: U256) -> RouteResult<U256> {
    if data.len() > 65_000 { return Err(invalid()); }
    let call = swapCall::abi_decode(data).map_err(|_| invalid())?;
    if call.abi_encode() != data.as_ref() { return Err(invalid()); }
    let e = &call.execution; let d = &e.desc;
    if d.srcToken != request.token_in || d.dstToken != request.token_out || d.amount != request.amount_in
        || (d.dstReceiver != request.executor && !d.dstReceiver.is_zero())
        || d.minReturnAmount.is_zero() || d.minReturnAmount < minimum || !d.permit.is_empty()
        || d.flags & U256::from(0x23) != U256::ZERO || !d.feeReceivers.is_empty() || !d.feeAmounts.is_empty()
        || e.callTarget.is_zero() || e.callTarget == request.executor || e.approveTarget == request.executor || e.targetData.is_empty()
        || d.srcReceivers.len() != d.srcAmounts.len() || d.srcReceivers.len() > 64
        || d.srcReceivers.iter().any(|v| v.is_zero() || *v == request.executor)
        || d.srcAmounts.iter().try_fold(U256::ZERO, |sum, amount| sum.checked_add(*amount)) != Some(d.amount) { return Err(invalid()); }
    Ok(d.minReturnAmount)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> ExactInputRequest {
        ExactInputRequest { token_in:Address::repeat_byte(1),token_out:Address::repeat_byte(2),
            amount_in:U256::from(100),executor:Address::repeat_byte(3),at:barterlend_state::BlockRef {
                number:10,hash:B256::repeat_byte(10),parent_hash:B256::repeat_byte(9),timestamp:100,
                commitment:barterlend_state::Commitment::Latest } }
    }
    fn call(r:&ExactInputRequest)->swapCall {
        swapCall { execution:Execution { callTarget:Address::repeat_byte(4),approveTarget:Address::ZERO,
            targetData:Bytes::from(vec![1]),clientData:Bytes::new(),desc:Description {
                srcToken:r.token_in,dstToken:r.token_out,srcReceivers:vec![Address::repeat_byte(4)],
                srcAmounts:vec![r.amount_in],feeReceivers:vec![],feeAmounts:vec![],dstReceiver:r.executor,
                amount:r.amount_in,minReturnAmount:U256::from(198),flags:U256::ZERO,permit:Bytes::new() } } }
    }
    #[test]
    fn validates_erc20_exact_input_and_rejects_adversarial_payloads() {
        let r=request();let router=Address::repeat_byte(5);let good=call(&r);
        let build=|data:Bytes| json!({"routerAddress":router,"amountIn":"100","amountOut":"200","data":data,"value":"0"});
        assert!(validate_build(&build(good.abi_encode().into()),&r,router,100).is_ok());
        let mutations:Vec<fn(&mut swapCall)>=vec![
            |c|c.execution.desc.srcToken=Address::ZERO, |c|c.execution.desc.dstToken=Address::ZERO,
            |c|c.execution.desc.dstReceiver=Address::repeat_byte(9),|c|c.execution.desc.amount+=U256::ONE,
            |c|c.execution.desc.minReturnAmount-=U256::ONE,|c|c.execution.desc.flags=U256::ONE,
            |c|c.execution.desc.flags=U256::from(2),|c|c.execution.desc.flags=U256::from(32),
            |c|c.execution.desc.permit=Bytes::from(vec![1]),|c|c.execution.desc.feeReceivers.push(Address::repeat_byte(9)),
            |c|c.execution.desc.feeAmounts.push(U256::ONE),|c|c.execution.desc.srcAmounts[0]-=U256::ONE,
            |c|c.execution.desc.srcReceivers[0]=Address::ZERO,|c|c.execution.callTarget=Address::ZERO,
            |c|c.execution.approveTarget=Address::repeat_byte(3),|c|c.execution.targetData=Bytes::new(),
        ];
        for (i,mutate) in mutations.iter().enumerate(){let mut c=call(&r);mutate(&mut c);
            assert!(validate_build(&build(c.abi_encode().into()),&r,router,100).is_err(),"mutation {i}");}
        let mut trailing=good.abi_encode();trailing.push(0);assert!(validate_build(&build(trailing.into()),&r,router,100).is_err());
        let mut value=build(good.abi_encode().into());value["value"]=json!("1");assert!(validate_build(&value,&r,router,100).is_err());
        let path:Bytes=(MAGIC,router,Bytes::from(good.abi_encode())).abi_encode().into();
        assert!(validate_path(&path,&r,router,U256::from(198)).is_ok());
        assert!(validate_path(&path,&r,Address::ZERO,U256::from(198)).is_err());
    }
    #[tokio::test]
    async fn api_retry_is_bounded_rate_limited_and_never_leaks_response() {
        use tokio::io::{AsyncReadExt,AsyncWriteExt};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let server=tokio::spawn(async move {
            let mut stamps=Vec::new();
            for status in [429,200,200] {
                let(mut socket,_)=listener.accept().await.unwrap();let mut buf=[0;4096];let mut length=0;
                while !buf[..length].windows(4).any(|s|s==b"\r\n\r\n") {
                    assert!(length<buf.len(),"test request headers exceed bound");
                    let read=socket.read(&mut buf[length..]).await.unwrap();assert!(read>0);length+=read;
                }
                stamps.push(Instant::now());let body=if stamps.len()==3 {"secret-provider-body"}else{"{\"code\":0,\"data\":{\"ok\":true}}"};
                let response=format!("HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}",body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            stamps
        });
        let chain=ChainIdentity {chain_id:31337,genesis_hash:B256::repeat_byte(1)};
        let config=KyberConfig {base_url:base.clone(),router:Address::repeat_byte(5),router_code_hash:B256::repeat_byte(5),client_id:"test".into(),api_key_env:None,requests_per_second:3,ttl_seconds:10};
        let reader=Arc::new(HttpReader::new(&base,Default::default()).unwrap());
        let limits=RouteLimits {quote_ttl_seconds:10,max_age_blocks:20,max_candidates:16,max_quote_calls:16,slippage_bps:100};
        let p=KyberProvider::new(reader,chain,config,&limits).unwrap();
        assert_eq!(p.request(p.client.get(&base)).await.unwrap()["ok"],true);
        let error=p.request(p.client.get(&base)).await.unwrap_err().to_string();assert!(!error.contains("secret-provider-body"));
        let stamps=server.await.unwrap();assert!(stamps[1].duration_since(stamps[0])>=Duration::from_millis(320));
        assert!(stamps[2].duration_since(stamps[1])>=Duration::from_millis(320));
    }
}
