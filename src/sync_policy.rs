//! Bounded background verification of current debt state held in RAM.

use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncPolicy {
    /// Background reconciliation is independent of the number of mined blocks.
    pub reconciliation_interval_seconds: u64,
    pub reconcile_positions_per_round: usize,
    /// Zero disables redundant user census after complete initial event discovery.
    pub reconcile_user_pages_per_round: usize,
    pub reconcile_user_page_size: u64,
    /// Overrides for the current-debt census. None uses SDK defaults; no historical replay.
    pub current_state: Option<barterlend_state::CurrentStateConfig>,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self { reconciliation_interval_seconds:60,
            reconcile_positions_per_round:2, reconcile_user_pages_per_round:0,
            reconcile_user_page_size:16,current_state:None }
    }
}

impl SyncPolicy {
    pub fn validate(&self) -> Result<()> {
        if let Some(policy)=&self.current_state {
            if policy.max_event_catchup_blocks==0 || !(1..=1_000_000).contains(&policy.max_calendar_markets) {
                return Err(eyre!("invalid bounded current-state policy"));
            }
        }
        if !(1..=3_600).contains(&self.reconciliation_interval_seconds)
            || self.reconcile_positions_per_round>64 || self.reconcile_user_pages_per_round>2
            || !(1..=128).contains(&self.reconcile_user_page_size) {
            return Err(eyre!("invalid bounded state synchronization policy"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_bounds_do_not_allow_unbounded_background_work() {
        SyncPolicy::default().validate().unwrap();
        for value in [0,3_601,u64::MAX] {
            let policy=SyncPolicy { reconciliation_interval_seconds:value,..Default::default() };
            assert!(policy.validate().is_err());
        }
        assert!(serde_json::from_str::<SyncPolicy>(r#"{"unexpected":1}"#).is_err());
    }
}
