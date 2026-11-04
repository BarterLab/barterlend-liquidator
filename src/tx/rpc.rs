//! Mutation transport is isolated from the read-only SDK and requires an explicit network profile.
use std::{sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{SystemTime, UNIX_EPOCH}};
use alloy::{consensus::{Transaction, TxEnvelope}, eips::{Decodable2718, Encodable2718}, primitives::{Address, B256, Bytes, U256}};
use async_trait::async_trait;
use barterlend_state::{BlockRef, ChainIdentity, Commitment, HttpReader, StateReader};
use color_eyre::eyre::{Result, eyre};
use serde_json::{Value, json};
use super::{AttemptKind, PlanMeta, Receipt, ReceiptLog, SignedAttempt, UnsignedTx};
use crate::recovery::{EndpointHealth, RecoveryPolicy};

#[async_trait]
pub trait TxRpc: Send + Sync {
    async fn identity(&self) -> Result<ChainIdentity>;
    async fn code_hash(&self, address: Address, at: &BlockRef) -> Result<B256>;
    async fn head(&self, commitment: Commitment) -> Result<BlockRef>;
    async fn block(&self, number: u64) -> Result<Option<BlockRef>>;
    async fn receipt(&self, hash: B256) -> Result<Option<Receipt>>;
    async fn nonce(&self, signer: Address, pending: bool) -> Result<u64>;
    async fn balance(&self, signer: Address) -> Result<U256>;
    async fn gas_price(&self) -> Result<U256>;
    async fn simulate(&self, sender: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<Bytes>;
    async fn estimate(&self, sender: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<u64>;
    async fn send_raw(&self, raw: &Bytes) -> Result<B256>;
    /// A concrete transport can recheck immutable liquidation bounds after its
    /// own last identity/head reads. The default preserves existing test mocks.
    async fn send_attempt(&self, attempt: &SignedAttempt) -> Result<B256> { self.send_raw(&attempt.raw).await }
}

pub fn validate_loopback_url(url: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(url)?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some()
    { return Err(eyre!("local implementation accepts only credential-free HTTP loopback RPC")); }
    Ok(url)
}

pub const MONAD_TESTNET_CHAIN_ID: u64 = 10143;

pub fn validate_profile_url(profile: &str, chain_id: u64, url: &str) -> Result<reqwest::Url> {
    match (profile, chain_id) {
        ("local", 31337) | ("monad-testnet-proxy", MONAD_TESTNET_CHAIN_ID) => validate_loopback_url(url),
        ("monad-testnet", MONAD_TESTNET_CHAIN_ID) | ("monad", 143) => {
            let url = reqwest::Url::parse(url).map_err(|_| eyre!("invalid testnet RPC URL"))?;
            if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some()
                || url.query().is_some() || url.fragment().is_some()
                || !url.host_str().is_some_and(|domain| domain.contains('.') && !domain.contains(':') && domain.parse::<std::net::IpAddr>().is_err()
                    && !domain.ends_with(".localhost") && !domain.ends_with(".local"))
            { return Err(eyre!("testnet requires credential-free public HTTPS RPC; loopback proxy requires its explicit profile")); }
            Ok(url)
        },
        _ => Err(eyre!("only explicit local31337, Monad testnet10143 and Monad143 profiles are supported")),
    }
}

pub struct LocalTxRpc { urls: Vec<reqwest::Url>, client: reqwest::Client, next: AtomicU64, reader: Arc<HttpReader>, chain_id: u64, expected_identity: Option<ChainIdentity>, health: Arc<EndpointHealth> }

impl LocalTxRpc {
    pub fn new(url: &str, reader: Arc<HttpReader>) -> Result<Self> {
        let url = validate_loopback_url(url)?;
        Self::build(url, reader, 31337, None)
    }
    pub fn for_profile(url: &str, reader: Arc<HttpReader>, profile: &str, identity: ChainIdentity) -> Result<Self> {
        Self::for_endpoints(&[url.to_owned()], reader, profile, identity)
    }
    pub fn for_endpoints(urls: &[String], reader: Arc<HttpReader>, profile: &str, identity: ChainIdentity) -> Result<Self> {
        if identity.genesis_hash.is_zero() { return Err(eyre!("mutation transport requires pinned nonzero genesis")); }
        let urls = urls.iter().map(|url| validate_profile_url(profile, identity.chain_id, url)).collect::<Result<Vec<_>>>()?;
        if urls.len() != reader.rpc().endpoint_count() || urls.iter().enumerate().any(|(index,url)| !reader.rpc().endpoint_at_matches(index,url)) {
            return Err(eyre!("read and mutation RPC endpoint sets differ"));
        }
        Self::build_endpoints(urls, reader, identity.chain_id, Some(identity))
    }
    fn build(url: reqwest::Url, reader: Arc<HttpReader>, chain_id: u64, expected_identity: Option<ChainIdentity>) -> Result<Self> {
        if !reader.endpoint_matches(&url) { return Err(eyre!("read and mutation RPC endpoints differ")); }
        Self::build_endpoints(vec![url], reader, chain_id, expected_identity)
    }
    fn build_endpoints(urls: Vec<reqwest::Url>, reader: Arc<HttpReader>, chain_id: u64, expected_identity: Option<ChainIdentity>) -> Result<Self> {
        let health = Arc::new(EndpointHealth::new(RecoveryPolicy::default(), chain_id != 31337, urls.len()));
        Ok(Self { urls, client: reqwest::Client::builder().retry(reqwest::retry::never()).no_proxy().timeout(reader.rpc().config().request_timeout).redirect(reqwest::redirect::Policy::none()).build()?, next: AtomicU64::new(1), reader, chain_id, expected_identity, health })
    }
    /// Share the application's configured per-endpoint age/progress observations.
    /// Public transports already enforce the default policy before this is set.
    pub fn set_health_monitor(&mut self, health: Arc<EndpointHealth>) { self.health = health; }
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_at(self.reader.rpc().endpoint_index(), method, params).await
    }
    async fn call_at(&self, index: usize, method: &str, params: Value) -> Result<Value> {
        if !matches!(method, "eth_getTransactionReceipt" | "eth_getTransactionCount" | "eth_getBalance" | "eth_gasPrice" | "eth_call" | "eth_estimateGas" | "eth_sendRawTransaction") { return Err(eyre!("RPC method is not allowed")); }
        // Every POST, including sends and receipt polling, shares the reader's
        // admission budget. Mutation failures return to the in-memory worker;
        // this transport must never automatically resend a transaction.
        let _permit = self.reader.rpc().acquire_request_permit().await?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let url = self.urls.get(index).ok_or_else(|| eyre!("mutation RPC endpoint index invalid"))?.clone();
        let mut response = self.client.post(url).json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).send().await.map_err(|error| error.without_url())?;
        if !response.status().is_success() { return Err(eyre!("local RPC HTTP status {}", response.status().as_u16())); }
        let max = self.reader.rpc().config().max_response_bytes;
        if response.content_length().is_some_and(|n| n > max as u64) { return Err(eyre!("local RPC response too large")); }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.without_url())? { if body.len().checked_add(chunk.len()).is_none_or(|length| length > max) { return Err(eyre!("local RPC response too large")); } body.extend_from_slice(&chunk); }
        let value: Value = serde_json::from_slice(&body)?;
        if value.get("id") != Some(&json!(id)) || value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") { return Err(eyre!("invalid RPC response identity")); }
        if let Some(error) = value.get("error") { return Err(eyre!("local RPC {method} failed: {}", error.get("message").and_then(Value::as_str).unwrap_or("unknown error").chars().take(400).collect::<String>())); }
        value.get("result").cloned().ok_or_else(|| eyre!("RPC result missing"))
    }
    async fn send_raw_checked(&self, raw: &Bytes, meta: Option<&PlanMeta>) -> Result<B256> {
        let mut bytes = raw.as_ref();
        let decoded = TxEnvelope::decode_2718(&mut bytes)?;
        if !bytes.is_empty() || !matches!(decoded, TxEnvelope::Legacy(_)) || decoded.chain_id() != Some(self.chain_id)
            || decoded.encoded_2718().as_slice() != raw.as_ref()
        { return Err(eyre!("mutation transport accepts only canonical chain{} legacy transactions", self.chain_id)); }
        let index = self.reader.rpc().endpoint_index();
        self.identity().await?;
        let head = self.reader.head(Commitment::Latest).await?;
        if self.reader.rpc().endpoint_index() != index { return Err(eyre!("mutation RPC endpoint changed during send validation")); }
        if let Some(meta) = meta {
            if head.timestamp > meta.deadline || head.number > meta.max_valid_block
                || head.number < meta.as_of.number || head.timestamp < meta.as_of.timestamp
                || head.number == meta.as_of.number && head.hash != meta.as_of.hash
            { return Err(eyre!("signed liquidation bounds expired or changed before actual send")); }
        }
        let unix_seconds = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|_| eyre!("system clock precedes Unix epoch"))?.as_secs();
        self.health.observe(index, head, unix_seconds)?;
        // The exact endpoint validated above is captured for the HTTP mutation.
        // A later selector change cannot reroute these immutable signed bytes.
        Ok(serde_json::from_value(self.call_at(index, "eth_sendRawTransaction", json!([raw])).await?)?)
    }
    async fn bracketed(&self, method: &str, sender: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<Value> {
        self.reader.ensure_canonical(at).await?;
        let object = json!({"from":sender,"to":tx.to,"data":tx.input,"value":"0x0","nonce":format!("0x{:x}",tx.nonce),"gas":format!("0x{:x}",tx.gas_limit),"gasPrice":format!("{:#x}",tx.gas_price)});
        let result = self.call(method, json!([object,format!("0x{:x}",at.number)])).await;
        self.reader.ensure_canonical(at).await?;
        result
    }
}

pub fn quantity(value: &Value) -> Result<U256> {
    let s = value.as_str().ok_or_else(|| eyre!("RPC quantity is not string"))?;
    if !s.starts_with("0x") || s.len() < 3 || s.len() > 66 || (s.len() > 3 && s.as_bytes()[2] == b'0') { return Err(eyre!("invalid RPC quantity")); }
    Ok(U256::from_str_radix(&s[2..], 16)?)
}
pub fn quantity_u64(value: &Value) -> Result<u64> { u64::try_from(quantity(value)?).map_err(|_| eyre!("RPC quantity exceeds u64")) }

#[async_trait]
impl TxRpc for LocalTxRpc {
    async fn identity(&self) -> Result<ChainIdentity> {
        let identity = self.reader.identity().await?;
        if identity.chain_id != self.chain_id || self.expected_identity.is_some_and(|expected| expected != identity) {
            return Err(eyre!("mutation transport chain/genesis does not match explicit profile"));
        }
        Ok(identity)
    }
    async fn code_hash(&self, address: Address, at: &BlockRef) -> Result<B256> { Ok(alloy::primitives::keccak256(self.reader.get_code(address, at).await?)) }
    async fn head(&self, c: Commitment) -> Result<BlockRef> { Ok(self.reader.head(c).await?) }
    async fn block(&self, n: u64) -> Result<Option<BlockRef>> { Ok(self.reader.block(n, Commitment::Latest).await?) }
    async fn receipt(&self, hash: B256) -> Result<Option<Receipt>> {
        let v = self.call("eth_getTransactionReceipt",json!([hash])).await?;
        if v.is_null() { return Ok(None); }
        let status = quantity_u64(&v["status"])?;
        if status > 1 { return Err(eyre!("unknown receipt status")); }
        let transaction_hash: B256 = serde_json::from_value(v["transactionHash"].clone())?;
        if transaction_hash != hash { return Err(eyre!("RPC receipt hash mismatch")); }
        let mut logs = Vec::new();
        for item in v["logs"].as_array().ok_or_else(|| eyre!("receipt logs missing"))? {
            if item["removed"] == json!(true) { return Err(eyre!("removed receipt log")); }
            if item["transactionHash"] != v["transactionHash"] || item["blockHash"] != v["blockHash"] || item["blockNumber"] != v["blockNumber"] { return Err(eyre!("receipt log anchor mismatch")); }
            logs.push(ReceiptLog { address: serde_json::from_value(item["address"].clone())?, topics: serde_json::from_value(item["topics"].clone())?, data: serde_json::from_value(item["data"].clone())? });
        }
        Ok(Some(Receipt { hash, block_number: quantity_u64(&v["blockNumber"])?, block_hash: serde_json::from_value(v["blockHash"].clone())?, succeeded: status == 1, effective_gas_price: quantity(&v["effectiveGasPrice"])?, gas_used: quantity_u64(&v["gasUsed"])?, logs }))
    }
    async fn nonce(&self, signer: Address, pending: bool) -> Result<u64> { quantity_u64(&self.call("eth_getTransactionCount",json!([signer,if pending {"pending"} else {"latest"}])).await?) }
    async fn balance(&self, signer: Address) -> Result<U256> { quantity(&self.call("eth_getBalance",json!([signer,"latest"])).await?) }
    async fn gas_price(&self) -> Result<U256> { quantity(&self.call("eth_gasPrice",json!([])).await?) }
    async fn simulate(&self, sender: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<Bytes> { Ok(serde_json::from_value(self.bracketed("eth_call",sender,tx,at).await?)?) }
    async fn estimate(&self, sender: Address, tx: &UnsignedTx, at: &BlockRef) -> Result<u64> { quantity_u64(&self.bracketed("eth_estimateGas",sender,tx,at).await?) }
    async fn send_raw(&self, raw: &Bytes) -> Result<B256> {
        self.send_raw_checked(raw, None).await
    }
    async fn send_attempt(&self, attempt: &SignedAttempt) -> Result<B256> {
        let meta = match &attempt.kind { AttemptKind::Liquidation(meta) => Some(meta), AttemptKind::Cancel => None };
        self.send_raw_checked(&attempt.raw, meta).await
    }
}
