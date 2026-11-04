use std::sync::Arc;
use barterlend_state::{StateReader, CurrentStateEngine, EngineConfig, PoolSpec, StateSnapshot, SyncStepOutcome};
use color_eyre::eyre::Result;
use crate::sync_policy::SyncPolicy;

/// A current-debt census followed by event invalidations; no historical replay or disk cache.
pub struct PoolSync<R: StateReader> { engine: CurrentStateEngine<R> }
impl<R: StateReader> PoolSync<R> {
    pub fn new(reader: Arc<R>, spec: PoolSpec, config: EngineConfig, policy: &SyncPolicy) -> Result<Self> {
        Ok(Self { engine: CurrentStateEngine::new(reader, spec, config, policy.current_state.clone().unwrap_or_default())? })
    }
    pub fn snapshot_shared(&self) -> Option<Arc<StateSnapshot>> { self.engine.snapshot_shared().map(|s| s.state) }
    pub async fn sync_step(&mut self) -> barterlend_state::Result<SyncStepOutcome> {
        Box::pin(self.engine.sync_step()).await.map(|s| s.outcome)
    }
}
