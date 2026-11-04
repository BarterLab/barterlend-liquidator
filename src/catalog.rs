//! Factory discovery is read-only. New pools need no executor registration.
use std::{collections::{BTreeMap, BTreeSet}, sync::Arc, time::Duration};
use alloy::primitives::{Address, keccak256};
use barterlend_state::{BlockRef, Commitment, HttpReader, PoolSpec, StateReader, PinnedRead, factory::{CreditPoolFactory, FactorySpec}};
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use crate::{local_config::{LocalManifest, LocalPoolManifest}, executor_abi::LiquidationExecutor, diagnostics::{emit_error, emit_stdout, emit_stderr, safe_reason}};

/// Trusted CREATE-based v5 factories, including every pinned predecessor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub factories: Vec<FactorySpec>,
    pub poll_seconds: u64,
    pub max_creation_nonce: u64,
}
impl Default for DiscoveryConfig {
    fn default() -> Self { Self { factories: Vec::new(), poll_seconds: 60, max_creation_nonce: 4096 } }
}
impl DiscoveryConfig {
    pub fn validate(&self) -> Result<()> {
        if self.factories.is_empty() || self.factories.len()>8 || !(5..=3600).contains(&self.poll_seconds)
            || !(2..=65_536).contains(&self.max_creation_nonce) { return Err(eyre!("invalid factory discovery bounds")); }
        let addresses:BTreeSet<_>=self.factories.iter().map(|f|f.address).collect();
        if addresses.len()!=self.factories.len() { return Err(eyre!("duplicate factory")); }
        for factory in &self.factories {
            factory.validate()?;
            if !factory.previous_factory.is_zero() && !addresses.contains(&factory.previous_factory) { return Err(eyre!("factory predecessor must be explicitly pinned")); }
            let mut visited=BTreeSet::new();let mut address=factory.address;
            while !address.is_zero() {
                if !visited.insert(address) { return Err(eyre!("factory predecessor cycle")); }
                address=self.factories.iter().find(|f|f.address==address).ok_or_else(||eyre!("missing predecessor"))?.previous_factory;
            }
        }
        Ok(())
    }
}

pub async fn pin_factories(reader:&HttpReader, addresses:Vec<Address>, at:&BlockRef) -> Result<DiscoveryConfig> {
    let mut pending=addresses;let mut factories=BTreeMap::new();
    while let Some(address)=pending.pop() {
        if factories.contains_key(&address) { continue; }
        if address.is_zero() || factories.len()>=8 { return Err(eyre!("factory identity/count limit")); }
        let code=reader.get_code(address,at).await?;
        if code.is_empty() { return Err(eyre!("factory has no code")); }
        let previous_factory=reader.contract_call(address,&CreditPoolFactory::previousFactoryCall {},at).await?;
        factories.insert(address,FactorySpec { address, code_hash:keccak256(code), previous_factory });
        if !previous_factory.is_zero() { pending.push(previous_factory); }
    }
    let config=DiscoveryConfig { factories:factories.into_values().collect(),..Default::default() };
    config.validate()?;reader.ensure_canonical(at).await?;Ok(config)
}

/// Every configured discovery source must match a factory bound to the executor.
pub async fn verify_binding(reader:&HttpReader, manifest:&LocalManifest, at:&BlockRef) -> Result<()> {
    let Some(config)=&manifest.discovery else { return Ok(()); };
    let mut reads=vec![PinnedRead::Code {address:manifest.executor}];
    for factory in &config.factories {
        reads.push(PinnedRead::call(manifest.executor,&LiquidationExecutor::lendingFactoryCodeHashesCall {lendingFactory:factory.address}));
    }
    let values=reader.read_views(&reads,at).await?;
    if keccak256(&values[0])!=manifest.executor_code_hash {
        return Err(eyre!("executor bytecode mismatch"));
    }
    for (factory,value) in config.factories.iter().zip(&values[1..]) {
        if crate::abi_validation::decode_return::<LiquidationExecutor::lendingFactoryCodeHashesCall>(value)?!=factory.code_hash {
            return Err(eyre!("discovery factory is not bound to executor"));
        }
    }
    Ok(())
}

pub async fn tokens_verified(reader:&HttpReader, executor:Address, asset:Address, collateral:Address, at:&BlockRef) -> Result<bool> {
    let reads=reader.read_views(&[
        PinnedRead::call(executor,&LiquidationExecutor::verifiedTokensCall {token:asset}),
        PinnedRead::call(executor,&LiquidationExecutor::verifiedTokensCall {token:collateral}),
    ],at).await?;
    Ok(crate::abi_validation::decode_return::<LiquidationExecutor::verifiedTokensCall>(&reads[0])?
        && crate::abi_validation::decode_return::<LiquidationExecutor::verifiedTokensCall>(&reads[1])?)
}

async fn admitted_pool(reader:&HttpReader, manifest:&LocalManifest, address:Address, at:&BlockRef) -> Result<Option<LocalPoolManifest>> {
    let config=reader.contract_call(manifest.executor,&LiquidationExecutor::poolConfigCall {pool:address},at).await?;
    if !config.recognized { return Err(eyre!("factory pool not recognized by executor")); }
    if !tokens_verified(reader,manifest.executor,config.asset,config.collateral,at).await? {
        emit_stdout(&serde_json::json!({"event":"pool_discovered","pool":address,"status":"token_approval_required"}));
        return Ok(None);
    }
    let spec=PoolSpec {address,deployment_block:0,expected_version:5,deployment_block_hash:Some(manifest.chain.genesis_hash)};
    let pool=reader.describe_pool(&spec,at).await?;
    if pool.asset.address!=config.asset || pool.collateral.address!=config.collateral { return Err(eyre!("discovered pool/executor token mismatch")); }
    let code=reader.get_code(pool.oracle,at).await?;
    if code.is_empty() || pool.oracle.is_zero() { return Err(eyre!("discovered pool oracle has no code")); }
    Ok(Some(LocalPoolManifest {spec,code_hash:pool.code_hash,asset:pool.asset,collateral:pool.collateral,
        oracle:pool.oracle,oracle_code_hash:keccak256(code)}))
}

#[derive(Default)]
pub struct CatalogCursor {
    next: BTreeMap<Address,u64>,
    pending: BTreeSet<Address>,
    last_anchor: Option<BlockRef>,
    pending_after: Option<Address>,
}
impl CatalogCursor {
    /// One bounded page per factory. The caller publishes only a successful pass.
    pub async fn step(&mut self, reader:&HttpReader, manifest:&mut LocalManifest) -> Result<bool> {
        let Some(config)=manifest.discovery.clone() else { return Ok(true); };
        config.validate()?;
        if reader.identity().await?!=manifest.chain { return Err(eyre!("factory chain identity mismatch")); }
        let at=reader.head(Commitment::Finalized).await?;
        verify_binding(reader,manifest,&at).await?;
        if let Some(previous)=self.last_anchor {
            if at.number<previous.number || (at.number==previous.number && at.hash!=previous.hash) {
                // Never carry cursor progress across a contradictory finalized anchor.
                self.next.clear();self.pending.clear();self.last_anchor=None;self.pending_after=None;
                return Err(eyre!("factory finalized anchor regressed"));
            }
        }
        let mut next=self.next.clone();let mut pending=self.pending.clone();let mut complete=true;
        for factory in &config.factories {
            let start=*next.get(&factory.address).unwrap_or(&1);
            let page=reader.factory_page(factory,start,32,&at).await?;
            if page.account_nonce>config.max_creation_nonce { return Err(eyre!("factory creation limit reached")); }
            pending.extend(page.pools);next.insert(factory.address,page.next_nonce);
            complete &= page.next_nonce==page.account_nonce;
        }
        for pool in manifest.pools.values() { pending.remove(&pool.spec.address); }
        if pending.len()+manifest.pools.len()>64 { return Err(eyre!("automatic pool catalog exceeds 64 pools")); }
        // Bound metadata work as well as CREATE enumeration. A large factory
        // batch must make progress within the outer timeout rather than restart
        // the same expensive pass forever.
        let remaining:Vec<_>=pending.iter().copied().filter(|a|self.pending_after.is_none_or(|last|*a>last)).collect();
        let probes:Vec<_>=remaining.iter().copied().take(4).collect();
        let pending_after=if remaining.len()>4 { probes.last().copied() } else { None };
        complete &= pending_after.is_none();
        let mut additions=Vec::new();
        for address in &probes {
            // A malformed or slow candidate must not prevent healthy pools from being admitted.
            match tokio::time::timeout(Duration::from_secs(10),admitted_pool(reader,manifest,*address,&at)).await {
                Ok(Ok(Some(pool)))=>additions.push(pool),
                Ok(Ok(None))=>{},
                Ok(Err(error))=>emit_stderr(&serde_json::json!({"event":"pool_discovery_candidate_retry","pool":address,"reason":safe_reason(&error)})),
                Err(_)=>emit_stdout(&serde_json::json!({"event":"pool_discovery_candidate_retry","pool":address,"reason":"timeout"})),
            }
        }
        reader.ensure_canonical(&at).await?;
        let mut updated=manifest.clone();
        for pool in additions {
            pending.remove(&pool.spec.address);
            updated.v3.allowed_tokens.extend([pool.asset.address,pool.collateral.address]);
            updated.pools.insert(pool.spec.address.to_string(),pool);
        }
        updated.validate()?;
        self.next=next;self.pending=pending;self.last_anchor=Some(at);self.pending_after=pending_after;*manifest=updated;Ok(complete)
    }
}

pub async fn watch_catalog(reader:Arc<HttpReader>, tx:watch::Sender<LocalManifest>, mut stop:watch::Receiver<bool>) {
    let mut cursor=CatalogCursor::default();
    loop {
        if *stop.borrow() || tx.is_closed() { break; }
        let mut manifest=tx.borrow().clone();let old=manifest.pools.len();
        let result=tokio::select! {
            biased;
            _=stop.changed()=>break,
            result=tokio::time::timeout(Duration::from_secs(60),cursor.step(&reader,&mut manifest))=>result,
        };
        let delay=match result {
            Ok(Ok(complete))=>{
                if manifest.pools.len()!=old {
                    emit_stdout(&serde_json::json!({"event":"pool_catalog_updated","pools":manifest.pools.len()}));
                    let _=tx.send(manifest.clone());
                }
                if complete { manifest.discovery.as_ref().map_or(60,|c|c.poll_seconds) } else { 1 }
            },
            Ok(Err(error))=>{ emit_error("pool_discovery_retry",&error,None);10 },
            Err(_)=>{ emit_stdout(&serde_json::json!({"event":"pool_discovery_retry","reason":"timeout"}));10 },
        };
        tokio::select! { _=stop.changed()=>break,_=tokio::time::sleep(Duration::from_secs(delay))=>{} }
    }
}
