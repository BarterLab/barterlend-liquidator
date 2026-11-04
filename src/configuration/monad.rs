//! Reviewed Monad deployments. Discovery only reads existing owner permissions.
use super::*;
use crate::routes::{V4Config, kyber::KyberConfig, uniswap_v4::V4RouteProvider};
use serde::Deserialize;

sol! { interface IntermediatePermission { function intermediates(address token) external view returns (bool); } }

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V3Deployments {
    pub factory: Address, pub router: Address, pub quoter: Address, pub wrapped_native: Address,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preset {
    pub chain: barterlend_state::ChainIdentity,
    pub v3: V3Deployments,
    pub v3_code_hashes: V3CodeHashes,
    pub v4: V4Config,
    pub kyber: KyberConfig,
    pub intermediates: Vec<Address>,
}
impl Preset {
    pub fn load() -> Result<Self> { Ok(serde_json::from_str(include_str!("monad.json"))?) }
}

pub(super) fn require(ok: bool, reason: &'static str) -> Result<()> {
    if !ok { return Err(eyre!(reason)); }
    Ok(())
}

pub(super) async fn configure(reader: &Arc<HttpReader>, manifest: &mut LocalManifest,
    preset: Preset, at: &barterlend_state::BlockRef) -> Result<()> {
    let common=&manifest.v3;
    require(common.chain == preset.chain
        && common.factory == preset.v3.factory && common.router == preset.v3.router
        && common.wrapped_native == preset.v3.wrapped_native
        && common.code_hashes.factory == preset.v3_code_hashes.factory
        && common.code_hashes.router == preset.v3_code_hashes.router
        && common.code_hashes.wrapped_native == preset.v3_code_hashes.wrapped_native
        && (common.quoter != preset.v3.quoter || common.code_hashes.quoter == preset.v3_code_hashes.quoter),
        "monad_config_v3_deployment_mismatch")?;
    require(reader.contract_call(manifest.executor,&LiquidationExecutor::v4ManagerCall {},at).await? == preset.v4.manager,
        "monad_config_v4_manager_not_configured")?;
    require(reader.contract_call(manifest.executor,&LiquidationExecutor::kyberRouterCall {},at).await? == preset.kyber.router
        && reader.contract_call(manifest.executor,&LiquidationExecutor::kyberRouterCodeHashCall {},at).await? == preset.kyber.router_code_hash,
        "monad_config_kyber_not_configured")?;
    require(code_hash(reader,preset.kyber.router,at).await? == preset.kyber.router_code_hash,
        "monad_config_kyber_code_mismatch")?;

    let mut tokens=BTreeSet::from_iter(preset.v4.pools.iter().flat_map(|p|[p.currency0,p.currency1]));
    tokens.extend(preset.intermediates.iter().copied());
    tokens.extend(manifest.v3.intermediates.iter().copied());
    let mut verified=BTreeMap::new();
    for token in tokens {
        verified.insert(token,reader.contract_call(manifest.executor,&LiquidationExecutor::verifiedTokensCall {token},at).await?);
    }
    let mut intermediates=BTreeSet::new();
    for token in manifest.v3.intermediates.iter().chain(&preset.intermediates).copied().collect::<BTreeSet<_>>() {
        let enabled=verified[&token]
            && reader.contract_call(manifest.executor,&IntermediatePermission::intermediatesCall {token},at).await?;
        if manifest.v3.intermediates.contains(&token) {
            require(enabled,"monad_config_intermediate_not_approved")?;
        }
        if enabled { intermediates.insert(token); }
    }
    let mut v4=preset.v4;
    let mut pools=Vec::new();
    for key in &v4.pools {
        if verified[&key.currency0] && verified[&key.currency1]
            && reader.contract_call(manifest.executor,&LiquidationExecutor::allowedV4PoolsCall {id:key.id()?},at).await? {
            pools.push(key.clone());
        }
    }
    require(!pools.is_empty(),"monad_config_no_approved_v4_pools")?;
    v4.pools=pools;
    manifest.v3.intermediates=intermediates.into_iter().collect();
    manifest.v3.allowed_tokens.extend(manifest.v3.intermediates.iter().copied());
    manifest.v3.allowed_tokens.extend(v4.pools.iter().flat_map(|p|[p.currency0,p.currency1]));
    // Includes expected manager/quoter code hashes and the quoter's manager binding.
    V4RouteProvider::new(reader.clone(),v4.clone(),manifest.v3.clone())?.verify_at(at).await?;
    manifest.v4=Some(v4);
    manifest.kyber=Some(preset.kyber);
    Ok(())
}
