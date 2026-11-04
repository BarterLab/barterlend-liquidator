//! Bounded test-stack control CLI, separate from the liquidator runtime.
use std::{fs::{self, File, OpenOptions}, io::{Read, Write}, path::{Path, PathBuf}, sync::atomic::{AtomicU64, Ordering}, time::{Duration, Instant}};
use alloy::{consensus::{TxLegacy, TxEnvelope, Transaction, SignableTransaction, transaction::SignerRecoverable}, eips::{Encodable2718, Decodable2718}, primitives::{Address, B256, Bytes, TxKind, U256, keccak256}, signers::{Signer, local::PrivateKeySigner}};
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_GAS: u64 = 30_000_000;
const MAX_INITCODE: usize = 256 * 1024;
const MAX_BUDGET: u128 = 15_000_000_000_000_000_000;
static TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "https://testnet-rpc.monad.xyz")] rpc: String,
    #[arg(long, default_value_t = 10143)] chain_id: u64,
    #[arg(long, default_value_t = Address::ZERO)] owner: Address,
    /// Required for public networks; obtain the genesis hash with a read-only request first.
    #[arg(long)] genesis: Option<B256>,
    #[arg(long)] journal: PathBuf,
    #[arg(long)] key_file: Option<PathBuf>,
    #[arg(long)] local: bool,
    #[arg(long)] testnet_proxy: bool,
    #[arg(long, default_value = "0xd02ab486cedc0000")] budget_wei: U256,
    #[arg(long, default_value = "0x2c68af0bb140000")] reserve_wei: U256,
    #[arg(long, default_value = "0xe8d4a51000")] max_gas_price_wei: U256,
    #[arg(long, default_value_t = 120)] wait_seconds: u64,
    #[command(subcommand)] command: Command,
}
#[derive(Subcommand)]
enum Command {
    GenerateKeyFile { #[arg(long)] output: PathBuf },
    KeyAddress { #[arg(long)] input: PathBuf },
    Inspect,
    Status,
    Call { #[arg(long)] to: Address, #[arg(long)] data: Bytes },
    Simulate { #[arg(long)] plan: PathBuf },
    Send { #[arg(long)] plan: PathBuf },
    Deploy { #[arg(long)] id: String, #[arg(long)] artifact: PathBuf, #[arg(long)] artifact_keccak: B256, #[arg(long)] constructor_file: PathBuf, #[arg(long)] expected_runtime_hash: Option<B256> },
    Resume,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EventAssertion { address: Address, topic0: B256, #[serde(default = "one")] min_count: usize }
fn one() -> usize { 1 }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CallAssertion { to: Address, data: Bytes, expected_result: Bytes }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Plan {
    id: String,
    to: Option<Address>,
    input: Bytes,
    #[serde(default)] value: U256,
    #[serde(default = "max_gas")] max_gas: u64,
    #[serde(default)] expected_contract: Option<Address>,
    #[serde(default)] expected_runtime_hash: Option<B256>,
    #[serde(default)] events: Vec<EventAssertion>,
    #[serde(default)] before: Vec<CallAssertion>,
    #[serde(default)] after: Vec<CallAssertion>,
}
fn max_gas() -> u64 { MAX_GAS }
impl Plan {
    fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.id.len() > 120 || !self.id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) { return Err(eyre!("invalid plan id")); }
        if self.max_gas < 21_000 || self.max_gas > MAX_GAS || self.input.len() > MAX_INITCODE || self.events.len() > 64 || self.before.len() > 32 || self.after.len() > 32 { return Err(eyre!("plan exceeds bounded limits")); }
        if self.to.is_none() && self.input.is_empty() { return Err(eyre!("empty creation code")); }
        if self.to.is_some() && self.expected_contract.is_some() { return Err(eyre!("call cannot assert creation address")); }
        if self.events.iter().any(|e| e.min_count == 0 || e.min_count > 1024) { return Err(eyre!("invalid event assertion count")); }
        if self.before.iter().chain(&self.after).any(|a| a.data.len() > 65_536 || a.expected_result.len() > 262_144) { return Err(eyre!("call assertion too large")); }
        Ok(())
    }
    fn hash(&self) -> Result<B256> { Ok(keccak256(serde_json::to_vec(self)?)) }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Identity { chain_id: u64, genesis: B256, owner: Address, endpoint_hash: B256, budget_wei: U256, reserve_wei: U256, max_gas_price_wei: U256 }
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry { plan: Plan, nonce: u64, gas_price: u128, gas_limit: u64, hash: B256, raw: Bytes, receipt: Option<Value>, validated: bool }
impl Entry {
    fn bound(&self) -> Result<U256> { U256::from(self.gas_limit).checked_mul(U256::from(self.gas_price)).and_then(|fee| fee.checked_add(self.plan.value)).ok_or_else(|| eyre!("transaction cost overflow")) }
    fn validate(&self, identity: &Identity) -> Result<()> {
        self.plan.validate()?;
        let mut bytes = self.raw.as_ref();
        let tx = TxEnvelope::decode_2718(&mut bytes).map_err(|_| eyre!("invalid retained transaction"))?;
        if !bytes.is_empty() || !matches!(tx, TxEnvelope::Legacy(_)) || tx.encoded_2718().as_slice() != self.raw.as_ref()
            || tx.chain_id() != Some(identity.chain_id) || tx.nonce() != self.nonce || tx.gas_limit() != self.gas_limit
            || tx.gas_price() != Some(self.gas_price) || tx.to() != self.plan.to || tx.value() != self.plan.value || tx.input() != &self.plan.input
            || tx.recover_signer().map_err(|_| eyre!("retained signature invalid"))? != identity.owner || keccak256(&self.raw) != self.hash
            || self.gas_limit < 21_000 || self.gas_limit > self.plan.max_gas || U256::from(self.gas_price) > identity.max_gas_price_wei || self.gas_price == 0
        { return Err(eyre!("retained transaction identity mismatch")); }
        if self.plan.to.is_none() && self.plan.expected_contract != Some(identity.owner.create(self.nonce)) { return Err(eyre!("retained deployment address mismatch")); }
        if self.validated && self.receipt.is_none() { return Err(eyre!("validated result lacks receipt")); }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Journal { generation: u64, identity: Identity, entries: Vec<Entry> }
#[derive(Serialize, Deserialize)]
struct Disk { checksum: B256, journal: Journal }
struct Store { path: PathBuf, _lock: File, journal: Journal }

fn regular_read(path: &Path, max: u64, secret: bool) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path).map_err(|_| eyre!("input file unavailable"))?;
    if !before.file_type().is_file() || before.len() > max { return Err(eyre!("input must be a bounded regular file")); }
    let mut options=OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
    let file = options.open(path).map_err(|_| eyre!("input file unavailable"))?;
    let opened = file.metadata()?;
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() || (secret && opened.mode() & 0o077 != 0) { return Err(eyre!("input identity or private permissions invalid")); }
    }
    if !opened.is_file() { return Err(eyre!("input must be regular")); }
    let mut bytes = Vec::new(); file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max { return Err(eyre!("input exceeds limit")); }
    Ok(bytes)
}
fn load_key(path: &Path, owner: Address) -> Result<PrivateKeySigner> { barterlend_liquidator::operator_key::read_private_key_file(path, owner) }
fn ensure_parent(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() { return Err(eyre!("journal/key path must be absolute")); }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| eyre!("journal needs parent directory"))?;
    // Do not follow symlink ancestors into another repository or secret location.
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() { continue; }
        if let Ok(meta) = fs::symlink_metadata(ancestor) { if meta.file_type().is_symlink() || !meta.is_dir() { return Err(eyre!("journal parent must not contain symlinks")); } }
    }
    if parent.ancestors().any(|p|p.join(".git").exists()) || parent.starts_with(env!("CARGO_MANIFEST_DIR")) { return Err(eyre!("keep journals and keys outside repositories")); }
    fs::create_dir_all(parent)?;
    if parent.canonicalize()?.ancestors().any(|p|p.join(".git").exists()) { return Err(eyre!("keep journals and keys outside repositories")); }
    Ok(parent.to_owned())
}
fn reject_nonregular(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => Err(eyre!("journal/lock must be regular")),
        Ok(_) => Ok(()), Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), Err(_) => Err(eyre!("journal metadata unavailable")),
    }
}
impl Store {
    fn open(path: PathBuf, identity: Identity) -> Result<Self> {
        ensure_parent(&path)?; reject_nonregular(&path)?;
        let lock_path = path.with_file_name(format!("{}.lock", path.file_name().ok_or_else(|| eyre!("journal filename missing"))?.to_string_lossy()));
        reject_nonregular(&lock_path)?;
        let mut options = OpenOptions::new(); options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
        let lock = options.open(lock_path)?; lock.try_lock().map_err(|_| eyre!("testnet control journal already locked"))?;
        let journal = if path.exists() {
            let disk: Disk = serde_json::from_slice(&regular_read(&path, MAX_FILE, true)?).map_err(|_| eyre!("journal JSON invalid"))?;
            if disk.checksum != keccak256(serde_json::to_vec(&disk.journal)?) || disk.journal.identity != identity { return Err(eyre!("journal checksum/identity mismatch")); }
            disk.journal
        } else { Journal { generation: 0, identity, entries: vec![] } };
        let store = Self { path, _lock: lock, journal }; store.validate()?; Ok(store)
    }
    fn validate(&self) -> Result<()> {
        let mut ids = std::collections::BTreeSet::new();
        let mut previous = None;
        for (i, entry) in self.journal.entries.iter().enumerate() {
            entry.validate(&self.journal.identity)?;
            if !ids.insert(&entry.plan.id) || previous.is_some_and(|n: u64| n.checked_add(1) != Some(entry.nonce)) || (entry.receipt.is_none() && i + 1 != self.journal.entries.len()) { return Err(eyre!("journal has conflicting ids/nonces or unresolved prefix")); }
            previous = Some(entry.nonce);
        }
        if self.total_bound()? > self.journal.identity.budget_wei { return Err(eyre!("journal exceeds total budget")); }
        Ok(())
    }
    fn total_bound(&self) -> Result<U256> { self.journal.entries.iter().try_fold(U256::ZERO, |sum, e| sum.checked_add(e.bound()?).ok_or_else(|| eyre!("budget overflow"))) }
    fn commit(&mut self, next: Journal) -> Result<()> {
        if next.identity != self.journal.identity || next.generation != self.journal.generation + 1 || next.entries.len() < self.journal.entries.len() { return Err(eyre!("invalid journal transition")); }
        for (old, new) in self.journal.entries.iter().zip(&next.entries) {
            if old.plan != new.plan || old.nonce != new.nonce || old.gas_price != new.gas_price || old.gas_limit != new.gas_limit || old.hash != new.hash || old.raw != new.raw
                || (old.receipt.is_some() && old.receipt != new.receipt) || (old.validated && !new.validated) { return Err(eyre!("signed evidence is immutable")); }
        }
        let old = std::mem::replace(&mut self.journal, next); let validation = self.validate(); let next = std::mem::replace(&mut self.journal, old); validation?;
        let bytes = serde_json::to_vec(&Disk { checksum: keccak256(serde_json::to_vec(&next)?), journal: next.clone() })?;
        if bytes.len() as u64 > MAX_FILE { return Err(eyre!("journal capacity reached")); }
        reject_nonregular(&self.path)?;
        let parent = ensure_parent(&self.path)?;
        let temp = parent.join(format!(".testnet-ctl-{}-{}.tmp", std::process::id(), TEMP.fetch_add(1, Ordering::Relaxed)));
        let mut options = OpenOptions::new(); options.write(true).create_new(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&temp)?; file.write_all(&bytes)?; file.sync_all()?; fs::rename(&temp, &self.path)?; File::open(parent)?.sync_all()?;
        self.journal = next; Ok(())
    }
    fn append(&mut self, entry: Entry) -> Result<()> { let mut next = self.journal.clone(); next.generation = next.generation.checked_add(1).ok_or_else(|| eyre!("journal generation overflow"))?; next.entries.push(entry); self.commit(next) }
    fn finalize(&mut self, receipt: Value, validated: bool) -> Result<()> { let mut next = self.journal.clone(); next.generation = next.generation.checked_add(1).ok_or_else(|| eyre!("journal generation overflow"))?; let entry = next.entries.last_mut().ok_or_else(|| eyre!("no signed transaction"))?; entry.receipt = Some(receipt); entry.validated = validated; self.commit(next) }
}

struct Rpc { url: reqwest::Url, client: reqwest::Client, next: AtomicU64 }
impl Rpc {
    fn new(text: &str, local: bool) -> Result<Self> {
        let url = reqwest::Url::parse(text).map_err(|_| eyre!("invalid RPC URL"))?;
        let allowed = if local { url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1")) } else { url.scheme() == "https" && url.host_str().is_some() };
        if !allowed || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() { return Err(eyre!("RPC requires credential-free HTTPS or explicit local loopback")); }
        Ok(Self { url, client: reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(20)).build().map_err(|_| eyre!("RPC client creation failed"))?, next: AtomicU64::new(1) })
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut response = self.client.post(self.url.clone()).json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).send().await.map_err(|_| eyre!("RPC transport unavailable"))?;
        if !response.status().is_success() || response.content_length().is_some_and(|n| n > MAX_FILE) { return Err(eyre!("RPC HTTP status/size rejected")); }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| eyre!("RPC body unavailable"))? { if body.len() + chunk.len() > MAX_FILE as usize { return Err(eyre!("RPC body too large")); } body.extend_from_slice(&chunk); }
        let value: Value = serde_json::from_slice(&body).map_err(|_| eyre!("RPC JSON invalid"))?;
        if value["id"] != json!(id) || value["jsonrpc"] != json!("2.0") || value.get("error").is_some() { return Err(eyre!("RPC response rejected for {method}")); }
        value.get("result").cloned().ok_or_else(|| eyre!("RPC result missing"))
    }
    async fn block(&self, tag: Value) -> Result<Value> { let block = self.call("eth_getBlockByNumber", json!([tag, false])).await?; if block.is_null() { return Err(eyre!("RPC block missing")); } q64(&block["number"])?; hash(&block["hash"])?; Ok(block) }
    async fn nonce(&self, owner: Address, tag: &str) -> Result<u64> { q64(&self.call("eth_getTransactionCount", json!([owner, tag])).await?) }
    async fn balance(&self, owner: Address) -> Result<U256> { q(&self.call("eth_getBalance", json!([owner, "latest"])).await?) }
    async fn assert_calls(&self, assertions: &[CallAssertion], tag: &str) -> Result<()> {
        for assertion in assertions { if self.call("eth_call", json!([{"to":assertion.to,"data":assertion.data}, tag])).await? != json!(assertion.expected_result) { return Err(eyre!("getter assertion failed")); } } Ok(())
    }
    async fn code_hash(&self, address: Address, tag: &str) -> Result<B256> { let bytes: Bytes = serde_json::from_value(self.call("eth_getCode", json!([address, tag])).await?).map_err(|_| eyre!("RPC code invalid"))?; if bytes.is_empty() { return Err(eyre!("contract runtime code empty")); } Ok(keccak256(bytes)) }
}
fn q(value: &Value) -> Result<U256> { barterlend_liquidator::tx::rpc::quantity(value).map_err(|_| eyre!("RPC quantity invalid")) }
fn q64(value: &Value) -> Result<u64> { u64::try_from(q(value)?).map_err(|_| eyre!("RPC quantity exceeds u64")) }
fn hash(value: &Value) -> Result<B256> { serde_json::from_value(value.clone()).map_err(|_| eyre!("RPC hash invalid")) }
fn plan_file(path: &Path) -> Result<Plan> { let plan: Plan = serde_json::from_slice(&regular_read(path, MAX_FILE, false)?).map_err(|_| eyre!("plan JSON invalid"))?; plan.validate()?; Ok(plan) }
fn object(owner: Address, plan: &Plan, nonce: u64, gas: u64, price: u128) -> Value {
    let mut tx = json!({"from":owner,"data":plan.input,"value":format!("{:#x}",plan.value),"nonce":format!("0x{nonce:x}"),"gas":format!("0x{gas:x}"),"gasPrice":format!("0x{price:x}")});
    if let Some(to) = plan.to { tx["to"] = json!(to); } tx
}
async fn prepare(rpc: &Rpc, store: &Store, mut plan: Plan) -> Result<(Plan, u64, u64, u128)> {
    check_identity(rpc, &store.journal.identity).await?;
    plan.validate()?;
    if store.journal.entries.last().is_some_and(|entry| entry.receipt.is_none() || !entry.validated) { return Err(eyre!("previous transaction unresolved; use resume")); }
    let owner = store.journal.identity.owner;
    let nonce = rpc.nonce(owner, "latest").await?;
    if rpc.nonce(owner, "pending").await? != nonce || store.journal.entries.last().is_some_and(|entry| entry.nonce.checked_add(1) != Some(nonce)) { return Err(eyre!("owner nonce advanced externally or pending transaction untracked")); }
    if plan.to.is_none() {
        let address = owner.create(nonce);
        if plan.expected_contract.is_some_and(|expected| expected != address) { return Err(eyre!("deployment address assertion failed")); }
        plan.expected_contract = Some(address);
    }
    let head = rpc.block(json!("latest")).await?; let tag = head["number"].as_str().ok_or_else(|| eyre!("block tag missing"))?;
    rpc.assert_calls(&plan.before, tag).await?;
    if let (Some(to), Some(expected)) = (plan.to, plan.expected_runtime_hash) { if rpc.code_hash(to, tag).await? != expected { return Err(eyre!("target runtime hash assertion failed")); } }
    let price = q(&rpc.call("eth_gasPrice", json!([])).await?)?;
    if price.is_zero() || price > store.journal.identity.max_gas_price_wei { return Err(eyre!("gas price cap exceeded")); }
    let price = u128::try_from(price).map_err(|_| eyre!("gas price exceeds uint128"))?;
    let balance = rpc.balance(owner).await?;
    let available = balance.checked_sub(store.journal.identity.reserve_wei).ok_or_else(|| eyre!("native reserve unavailable"))?;
    let remaining = store.journal.identity.budget_wei.checked_sub(store.total_bound()?).ok_or_else(|| eyre!("total budget exhausted"))?.min(available);
    let fee_budget = remaining.checked_sub(plan.value).ok_or_else(|| eyre!("native value exceeds remaining budget"))?;
    let gas = u64::try_from(fee_budget / U256::from(price)).unwrap_or(u64::MAX).min(plan.max_gas).min(q64(&head["gasLimit"])?);
    if gas < 21_000 { return Err(eyre!("insufficient balance/budget for transaction")); }
    let estimate = q64(&rpc.call("eth_estimateGas", json!([object(owner, &plan, nonce, gas, price), tag])).await?)?;
    let signed_gas = estimate.checked_add(estimate / 5).and_then(|n| n.checked_add(10_000)).ok_or_else(|| eyre!("gas buffer overflow"))?;
    if signed_gas > gas { return Err(eyre!("estimated gas with buffer exceeds caps/balance")); }
    rpc.call("eth_call", json!([object(owner, &plan, nonce, signed_gas, price), tag])).await?;
    if rpc.block(json!(tag)).await?["hash"] != head["hash"] || rpc.nonce(owner, "latest").await? != nonce || rpc.nonce(owner, "pending").await? != nonce { return Err(eyre!("simulation anchor/nonce changed")); }
    Ok((plan, nonce, signed_gas, price))
}
async fn signed(identity: &Identity, plan: Plan, nonce: u64, gas_limit: u64, gas_price: u128, signer: &PrivateKeySigner) -> Result<Entry> {
    let tx = TxLegacy { chain_id: Some(identity.chain_id), nonce, gas_price, gas_limit, to: plan.to.map(TxKind::Call).unwrap_or(TxKind::Create), value: plan.value, input: plan.input.clone() };
    let signature = signer.sign_hash(&tx.signature_hash()).await.map_err(|_| eyre!("transaction signing failed"))?;
    let raw: Bytes = tx.into_signed(signature).encoded_2718().into();
    let entry = Entry { plan, nonce, gas_price, gas_limit, hash: keccak256(&raw), raw, receipt: None, validated: false }; entry.validate(identity)?; Ok(entry)
}
async fn canonical_receipt(rpc: &Rpc, entry: &Entry, owner: Address) -> Result<Option<Value>> {
    let receipt = rpc.call("eth_getTransactionReceipt", json!([entry.hash])).await?;
    if receipt.is_null() { return Ok(None); }
    if hash(&receipt["transactionHash"])? != entry.hash || serde_json::from_value::<Address>(receipt["from"].clone()).ok() != Some(owner) || receipt["to"] != json!(entry.plan.to) || q64(&receipt["status"])? > 1 || q64(&receipt["gasUsed"])? > entry.gas_limit || q(&receipt["effectiveGasPrice"])? > U256::from(entry.gas_price) { return Err(eyre!("receipt envelope mismatch")); }
    let number = q64(&receipt["blockNumber"])?; let block_hash = hash(&receipt["blockHash"])?;
    let finalized = rpc.block(json!("finalized")).await?;
    if q64(&finalized["number"])? < number { return Ok(None); }
    if hash(&rpc.block(receipt["blockNumber"].clone()).await?["hash"])? != block_hash { return Err(eyre!("receipt block is not canonical finalized history")); }
    if rpc.block(finalized["number"].clone()).await?["hash"] != finalized["hash"] || rpc.call("eth_getTransactionReceipt", json!([entry.hash])).await? != receipt { return Err(eyre!("receipt/finalized anchor changed")); }
    let logs = receipt["logs"].as_array().ok_or_else(|| eyre!("receipt logs missing"))?;
    for log in logs { if log["removed"] == json!(true) || log["transactionHash"] != receipt["transactionHash"] || log["blockHash"] != receipt["blockHash"] || log["blockNumber"] != receipt["blockNumber"] { return Err(eyre!("receipt log anchor invalid")); } }
    Ok(Some(receipt))
}
async fn validate_result(rpc: &Rpc, entry: &Entry, receipt: &Value) -> Result<Value> {
    if q64(&receipt["status"])? != 1 { return Err(eyre!("transaction reverted; receipt retained, no new transactions allowed")); }
    let tag = receipt["blockNumber"].as_str().ok_or_else(|| eyre!("receipt block number missing"))?;
    let address = if entry.plan.to.is_none() { let deployed: Address = serde_json::from_value(receipt["contractAddress"].clone()).map_err(|_| eyre!("deployment receipt address missing"))?; if Some(deployed) != entry.plan.expected_contract { return Err(eyre!("deployment receipt address mismatch")); } Some(deployed) } else { entry.plan.to };
    let runtime_hash = if entry.plan.to.is_none() || entry.plan.expected_runtime_hash.is_some() { let actual = rpc.code_hash(address.ok_or_else(|| eyre!("contract address missing"))?, tag).await?; if entry.plan.expected_runtime_hash.is_some_and(|expected| actual != expected) { return Err(eyre!("finalized runtime code hash assertion failed")); } Some(actual) } else { None };
    let logs = receipt["logs"].as_array().ok_or_else(|| eyre!("receipt logs missing"))?;
    for event in &entry.plan.events { let count = logs.iter().filter(|log| log["address"] == json!(event.address) && log["topics"].get(0) == Some(&json!(event.topic0))).count(); if count < event.min_count { return Err(eyre!("required receipt event missing")); } }
    rpc.assert_calls(&entry.plan.after, tag).await?;
    Ok(json!({"id":entry.plan.id,"hash":entry.hash,"nonce":entry.nonce,"block_number":q64(&receipt["blockNumber"])? ,"block_hash":receipt["blockHash"],"contract_address":if entry.plan.to.is_none() {address} else {None},"runtime_code_hash":runtime_hash,"gas_limit":entry.gas_limit,"gas_used":q64(&receipt["gasUsed"])? ,"gas_price":format!("0x{:x}",entry.gas_price),"signed_fee_bound_wei":U256::from(entry.gas_limit)*U256::from(entry.gas_price),"value":entry.plan.value,"finalized":true,"assertions_passed":true}))
}
async fn resume(rpc: &Rpc, store: &mut Store, seconds: u64) -> Result<Value> {
    check_identity(rpc, &store.journal.identity).await?;
    let entry = store.journal.entries.last().cloned().ok_or_else(|| eyre!("no signed transaction to resume"))?;
    if let Some(receipt) = &entry.receipt { verify_retained_receipt(rpc, &entry, store.journal.identity.owner).await?; let summary = validate_result(rpc, &entry, receipt).await?; if !entry.validated { store.finalize(receipt.clone(), true)?; } return Ok(summary); }
    let start = Instant::now(); let mut broadcast = false;
    loop {
        if let Some(receipt) = canonical_receipt(rpc, &entry, store.journal.identity.owner).await? {
            // Save the canonical receipt even when an assertion fails; never retry a mined nonce.
            store.finalize(receipt.clone(), false)?;
            let summary = validate_result(rpc, &entry, &receipt).await?;
            store.finalize(receipt, true)?; return Ok(summary);
        }
        let latest = rpc.nonce(store.journal.identity.owner, "latest").await?;
        let pending = rpc.nonce(store.journal.identity.owner, "pending").await?;
        if latest > entry.nonce {
            let found = rpc.call("eth_getTransactionReceipt", json!([entry.hash])).await?;
            if found.is_null() { return Err(eyre!("unknown transaction consumed signed nonce; journal retained")); }
        } else if latest < entry.nonce || entry.nonce.checked_add(1).is_none_or(|next| pending > next) { return Err(eyre!("owner nonce changed unexpectedly; journal retained")); }
        else if !broadcast {
            check_identity(rpc, &store.journal.identity).await?;
            let known=rpc.call("eth_getTransactionByHash",json!([entry.hash])).await?;
            if !known.is_null() {
                if hash(&known["hash"])? != entry.hash || q64(&known["nonce"])? != entry.nonce || serde_json::from_value::<Address>(known["from"].clone()).ok() != Some(store.journal.identity.owner) { return Err(eyre!("tracked pending transaction identity mismatch")); }
                broadcast=true;
            } else {
            if pending != entry.nonce { return Err(eyre!("unknown pending transaction occupies signed nonce; journal retained")); }
            let needed = entry.bound()?.checked_add(store.journal.identity.reserve_wei).ok_or_else(|| eyre!("reserve overflow"))?;
            if rpc.balance(store.journal.identity.owner).await? < needed { return Err(eyre!("insufficient balance/reserve for retained transaction")); }
            let head=rpc.block(json!("latest")).await?; let tag=head["number"].as_str().ok_or_else(||eyre!("latest block tag missing"))?;
            rpc.assert_calls(&entry.plan.before,tag).await?;
            if let (Some(to),Some(expected))=(entry.plan.to,entry.plan.expected_runtime_hash) { if rpc.code_hash(to,tag).await? != expected { return Err(eyre!("retained target runtime changed")); } }
            let call=object(store.journal.identity.owner,&entry.plan,entry.nonce,entry.gas_limit,entry.gas_price);
            if q64(&rpc.call("eth_estimateGas",json!([call.clone(),tag])).await?)? > entry.gas_limit { return Err(eyre!("retained signed gas no longer sufficient")); }
            rpc.call("eth_call",json!([call,tag])).await?;
            if rpc.block(json!(tag)).await?["hash"] != head["hash"] || rpc.nonce(store.journal.identity.owner,"latest").await? != entry.nonce || rpc.nonce(store.journal.identity.owner,"pending").await? != entry.nonce { return Err(eyre!("retained simulation anchor/nonce changed")); }
            let result = rpc.call("eth_sendRawTransaction", json!([entry.raw])).await;
            match result { Ok(sent) if hash(&sent)? == entry.hash => {}, Ok(_) => return Err(eyre!("broadcast returned another hash; journal retained")), Err(_) => return Err(eyre!("broadcast uncertain; resume the same durable transaction")) }
            broadcast = true;
            }
        }
        if start.elapsed() >= Duration::from_secs(seconds) { return Err(eyre!("finality wait timed out; resume the same durable transaction")); }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn check_identity(rpc: &Rpc, identity: &Identity) -> Result<()> {
    if q64(&rpc.call("eth_chainId", json!([])).await?)? != identity.chain_id || hash(&rpc.block(json!("0x0")).await?["hash"])? != identity.genesis { return Err(eyre!("RPC chain/genesis changed; send forbidden")); }
    if rpc.call("eth_getCode", json!([identity.owner,"latest"])).await? != json!("0x") { return Err(eyre!("operator must be an undelegated EOA; EIP7702/code account forbidden")); }
    Ok(())
}
async fn verify_retained_receipt(rpc: &Rpc, entry: &Entry, owner: Address) -> Result<()> {
    if canonical_receipt(rpc,entry,owner).await?.as_ref() != entry.receipt.as_ref() { return Err(eyre!("retained receipt no longer canonical finalized")); } Ok(())
}
fn generate_key_file(path: &Path) -> Result<Value> {
    if !path.is_absolute() { return Err(eyre!("new key file requires absolute path")); }
    ensure_parent(path)?;
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        let parent=fs::metadata(path.parent().ok_or_else(|| eyre!("key parent missing"))?)?;
        if parent.mode() & 0o077 != 0 || parent.uid() != unsafe { libc::geteuid() } { return Err(eyre!("generated key parent requires owned private0700 directory")); }
    }
    if fs::symlink_metadata(path).is_ok() { return Ok(json!({"address":barterlend_liquidator::operator_key::private_key_file_address(path)?,"key_file":path,"created":false})); }
    let signer=PrivateKeySigner::random();
    let secret=zeroize::Zeroizing::new(signer.to_bytes().0);
    let text=zeroize::Zeroizing::new(alloy::primitives::hex::encode(secret.as_ref()));
    let mut options=OpenOptions::new(); options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
    let mut file=options.open(path).map_err(|_| eyre!("new private key file unavailable/already exists"))?; file.write_all(text.as_bytes())?; file.write_all(b"\n")?; file.sync_all()?; File::open(path.parent().ok_or_else(|| eyre!("key parent missing"))?)?.sync_all()?;
    Ok(json!({"address":signer.address(),"key_file":path,"created":true}))
}

async fn run(args: Args) -> Result<Value> {
    if let Command::GenerateKeyFile { output } = &args.command { return generate_key_file(output); }
    if let Command::KeyAddress { input } = &args.command { return Ok(json!({"address":barterlend_liquidator::operator_key::private_key_file_address(input)?})); }
    if args.owner.is_zero() { return Err(eyre!("an explicit nonzero --owner is required for network commands")); }
    if args.local && args.testnet_proxy || (args.local && args.chain_id != 31337) || (!args.local && (args.chain_id != 10143 || args.genesis.is_none())) { return Err(eyre!("only explicitly local31337 or pinned Monad Testnet10143 is permitted")); }
    if args.budget_wei.is_zero() || args.budget_wei > U256::from(MAX_BUDGET) || args.max_gas_price_wei.is_zero() || args.wait_seconds > 3600 { return Err(eyre!("invalid budget/fee/wait bounds")); }
    let rpc = Rpc::new(&args.rpc, args.local || args.testnet_proxy)?;
    let actual_chain = q64(&rpc.call("eth_chainId", json!([])).await?)?; let genesis = hash(&rpc.block(json!("0x0")).await?["hash"])?;
    if actual_chain != args.chain_id || args.genesis.is_some_and(|expected| expected != genesis) { return Err(eyre!("RPC chain/genesis mismatch")); }
    let identity = Identity { chain_id: actual_chain, genesis, owner: args.owner, endpoint_hash: keccak256(rpc.url.as_str()), budget_wei: args.budget_wei, reserve_wei:args.reserve_wei, max_gas_price_wei: args.max_gas_price_wei };
    check_identity(&rpc, &identity).await?;
    let signer = match &args.key_file { Some(path) => Some(load_key(path, args.owner)?), None => None };
    match &args.command {
        Command::Inspect => {
            let latest=rpc.block(json!("latest")).await?;
            return Ok(json!({"chain_id":actual_chain,"genesis":genesis,"owner":args.owner,"balance_wei":rpc.balance(args.owner).await?,"latest_nonce":rpc.nonce(args.owner,"latest").await?,"pending_nonce":rpc.nonce(args.owner,"pending").await?,"gas_price_wei":q(&rpc.call("eth_gasPrice",json!([])).await?)?,"latest_block":q64(&latest["number"])? ,"latest_timestamp":q64(&latest["timestamp"])? ,"finalized":rpc.block(json!("finalized")).await?["number"],"key_matches":signer.is_some()}));
        },
        Command::Call { to, data } => return Ok(json!({"to":to,"data":data,"result":rpc.call("eth_call",json!([{"to":to,"data":data},"latest"])).await?})),
        _ => {}
    }
    let mut store = Store::open(args.journal.clone(), identity)?;
    if matches!(&args.command, Command::Status) { return Ok(json!({"chain_id":actual_chain,"owner":args.owner,"generation":store.journal.generation,"total_reserved_wei":store.total_bound()?,"entries":store.journal.entries.iter().map(|e| json!({"id":e.plan.id,"hash":e.hash,"nonce":e.nonce,"has_receipt":e.receipt.is_some(),"validated":e.validated,"expected_contract":e.plan.expected_contract})).collect::<Vec<_>>()})); }
    if matches!(&args.command, Command::Resume) { return resume(&rpc, &mut store, args.wait_seconds).await; }
    let plan = match &args.command {
        Command::Send { plan } | Command::Simulate { plan } => plan_file(plan)?,
        Command::Deploy { id, artifact, artifact_keccak, constructor_file, expected_runtime_hash } => {
            let bytes = regular_read(artifact, MAX_FILE, false)?;
            if keccak256(&bytes) != *artifact_keccak { return Err(eyre!("artifact fingerprint mismatch")); }
            let artifact: Value = serde_json::from_slice(&bytes).map_err(|_| eyre!("artifact JSON invalid"))?;
            let bytecode = if artifact["bytecode"].is_string() { artifact["bytecode"].clone() } else { artifact["bytecode"]["object"].clone() };
            let mut input: Bytes = serde_json::from_value(bytecode).map_err(|_| eyre!("artifact creation bytecode invalid/unlinked"))?;
            let constructor = regular_read(constructor_file, MAX_FILE, false)?;
            let args: Bytes = std::str::from_utf8(&constructor).map_err(|_| eyre!("constructor bytes invalid"))?.trim().parse().map_err(|_| eyre!("constructor file requires hex bytes"))?;
            let mut full = input.to_vec(); full.extend_from_slice(&args); input = full.into();
            Plan { id:id.clone(), to:None, input, value:U256::ZERO, max_gas:MAX_GAS, expected_contract:None, expected_runtime_hash:*expected_runtime_hash, events:vec![], before:vec![], after:vec![] }
        },
        _ => return Err(eyre!("invalid command")),
    };
    if let Some(old) = store.journal.entries.iter().find(|entry| entry.plan.id == plan.id) {
        let mut supplied = plan.clone(); if supplied.to.is_none() && supplied.expected_contract.is_none() { supplied.expected_contract = old.plan.expected_contract; }
        if supplied != old.plan { return Err(eyre!("plan id already bound to different transaction")); }
        let receipt = old.receipt.as_ref().ok_or_else(|| eyre!("plan is already signed; use resume"))?;
        verify_retained_receipt(&rpc,old,args.owner).await?;
        return validate_result(&rpc, old, receipt).await;
    }
    let (plan, nonce, gas_limit, gas_price) = prepare(&rpc, &store, plan).await?;
    if matches!(&args.command, Command::Simulate { .. }) { return Ok(json!({"id":plan.id,"plan_hash":plan.hash()?,"nonce":nonce,"gas_limit":gas_limit,"gas_price":format!("0x{gas_price:x}"),"expected_contract":plan.expected_contract,"simulation_passed":true,"sent":false})); }
    let signer = signer.as_ref().ok_or_else(|| eyre!("send/deploy requires local key-file"))?;
    let entry = signed(&store.journal.identity, plan, nonce, gas_limit, gas_price, signer).await?;
    store.append(entry)?; // Durable signed bytes precede every broadcast.
    resume(&rpc, &mut store, args.wait_seconds).await
}
#[tokio::main]
async fn main() {
    let args=Args::parse();
    // Poll one persistent signal receiver before any RPC. Cancelling an in-flight send
    // retains its durable bytes; no following action is started by this process.
    let result=tokio::select! { biased; signal=tokio::signal::ctrl_c() => { signal.map_err(|_| eyre!("shutdown signal unavailable")).and(Err(eyre!("interrupted; any signed transaction remains in the durable journal"))) }, result=run(args) => result };
    match result { Ok(value) => println!("{value}"), Err(error) => { eprintln!("{}", error); std::process::exit(1); } }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(signer: &PrivateKeySigner) -> Identity { Identity { chain_id:31337, genesis:B256::repeat_byte(1), owner:signer.address(), endpoint_hash:B256::repeat_byte(2), budget_wei:U256::from(MAX_BUDGET), reserve_wei:U256::ZERO, max_gas_price_wei:U256::from(1_000_000_000_000u64) } }
    fn plan() -> Plan { Plan { id:"test".into(), to:Some(Address::repeat_byte(3)), input:Bytes::new(), value:U256::from(2), max_gas:MAX_GAS, expected_contract:None, expected_runtime_hash:None, events:vec![], before:vec![], after:vec![] } }
    #[test]
    fn endpoint_and_chain_scope_reject_unsafe_urls() {
        assert!(Rpc::new("http://127.0.0.1:8545",true).is_ok()); assert!(Rpc::new("https://testnet-rpc.monad.xyz",false).is_ok());
        for (url,local) in [("http://public.example",false),("https://secret:password@example.test",false),("https://example.test?key=x",false),("http://localhost:8545",true),("https://example.test",true)] { assert!(Rpc::new(url,local).is_err()); }
    }
    #[test]
    fn plans_reject_unknown_fields_and_over_limit_envelopes() {
        let mut value=serde_json::to_value(plan()).unwrap(); value["unexpected"]=json!(true); assert!(serde_json::from_value::<Plan>(value).is_err());
        let mut p=plan(); p.max_gas=MAX_GAS+1; assert!(p.validate().is_err()); p=plan(); p.id="../unsafe".into(); assert!(p.validate().is_err()); p=plan(); p.input=vec![0;MAX_INITCODE+1].into(); assert!(p.validate().is_err());
    }
    #[tokio::test]
    async fn signed_intent_survives_restart_and_is_immutable() {
        let signer:PrivateKeySigner="0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap();
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().canonicalize().unwrap().join("journal.json"); let id=identity(&signer);
        let entry=signed(&id,plan(),7,31_000,1_000,&signer).await.unwrap(); let raw=entry.raw.clone(); let hash=entry.hash;
        { let mut store=Store::open(path.clone(),id.clone()).unwrap(); store.append(entry).unwrap(); assert!(Store::open(path.clone(),id.clone()).is_err()); let mut mutated=store.journal.clone(); mutated.generation+=1; mutated.entries[0].gas_price+=1; assert!(store.commit(mutated).is_err()); }
        let store=Store::open(path.clone(),id.clone()).unwrap(); assert_eq!(store.journal.entries[0].raw,raw); assert_eq!(store.journal.entries[0].hash,hash);
        let mut wrong=id; wrong.chain_id=10143; assert!(Store::open(path,wrong).is_err());
    }
    #[tokio::test]
    async fn journal_cannot_drop_signed_pending_or_exceed_budget() {
        let signer:PrivateKeySigner="0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap(); let dir=tempfile::tempdir().unwrap();
        let mut store=Store::open(dir.path().canonicalize().unwrap().join("journal.json"),identity(&signer)).unwrap(); store.append(signed(&store.journal.identity,plan(),0,30_000,10,&signer).await.unwrap()).unwrap();
        let mut truncated=store.journal.clone(); truncated.generation+=1; truncated.entries.clear(); assert!(store.commit(truncated).is_err());
        let mut next=plan(); next.id="next".into(); assert!(store.append(signed(&store.journal.identity,next,1,30_000,10,&signer).await.unwrap()).is_err());
        let mut excess=plan(); excess.value=U256::from(MAX_BUDGET); let emptydir=tempfile::tempdir().unwrap(); let mut empty=Store::open(emptydir.path().canonicalize().unwrap().join("journal.json"),identity(&signer)).unwrap(); assert!(empty.append(signed(&empty.journal.identity,excess,0,30_000,10,&signer).await.unwrap()).is_err());
    }
    #[test]
    fn key_file_rejects_wrong_owner_symlink_and_insecure_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("key"); fs::write(&path,"0x0000000000000000000000000000000000000000000000000000000000000001\n").unwrap(); fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();
        let signer:PrivateKeySigner="0000000000000000000000000000000000000000000000000000000000000001".parse().unwrap(); assert!(load_key(&path,signer.address()).is_ok()); assert!(load_key(&path,Address::ZERO).is_err());
        symlink(&path,dir.path().join("link")).unwrap(); assert!(load_key(&dir.path().join("link"),signer.address()).is_err()); fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap(); assert!(load_key(&path,signer.address()).is_err());
    }
}
