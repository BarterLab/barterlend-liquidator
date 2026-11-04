//! Recovery policy for trusted RPC endpoints. Chain risk still uses block time;
//! local clocks certify data freshness and schedule bounded recovery work.
use std::{sync::Mutex,time::{Duration, Instant}};
use barterlend_state::BlockRef;
use serde::{Deserialize, Serialize};
use color_eyre::eyre::{Result, eyre};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryPolicy {
    pub max_head_age_seconds: u64,
    pub max_no_progress_seconds: u64,
    pub max_future_seconds: u64,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
}
impl Default for RecoveryPolicy {
    fn default() -> Self { Self { max_head_age_seconds:120,max_no_progress_seconds:30,max_future_seconds:15,retry_initial_ms:500,retry_max_ms:30_000 } }
}
impl RecoveryPolicy {
    pub fn validate(&self) -> Result<()> {
        if !(1..=3600).contains(&self.max_head_age_seconds) || !(1..=3600).contains(&self.max_no_progress_seconds)
            || self.max_future_seconds > 60 || !(100..=60_000).contains(&self.retry_initial_ms)
            || self.retry_max_ms < self.retry_initial_ms || self.retry_max_ms > 60_000 {
            return Err(eyre!("invalid bounded recovery policy"));
        }
        Ok(())
    }
}

pub struct HeadMonitor { policy: RecoveryPolicy, enforce: bool, last: Option<BlockRef>, progressed: Instant }
impl HeadMonitor {
    pub fn new(policy: RecoveryPolicy, enforce: bool) -> Self { Self { policy,enforce,last:None,progressed:Instant::now() } }
    pub fn reset(&mut self) { self.last=None;self.progressed=Instant::now(); }
    pub fn observe(&mut self, head: BlockRef, unix_seconds: u64) -> Result<()> { self.observe_at(head,unix_seconds,Instant::now()) }
    fn observe_at(&mut self, head: BlockRef, unix_seconds: u64, now: Instant) -> Result<()> {
        if self.enforce {
            if head.timestamp > unix_seconds.saturating_add(self.policy.max_future_seconds) { return Err(eyre!("RPC head is ahead of local clock tolerance")); }
            if unix_seconds.saturating_sub(head.timestamp) > self.policy.max_head_age_seconds { return Err(eyre!("RPC head exceeds wall-clock age budget")); }
        }
        if self.last.is_none_or(|last| head.number>last.number) { self.progressed=now;self.last=Some(head); }
        if self.enforce && now.saturating_duration_since(self.progressed) >= Duration::from_secs(self.policy.max_no_progress_seconds) {
            return Err(eyre!("RPC head stopped advancing"));
        }
        Ok(())
    }
}

pub struct EndpointHealth { monitors: Vec<Mutex<HeadMonitor>> }
impl EndpointHealth {
    pub fn new(policy:RecoveryPolicy,enforce:bool,count:usize) -> Self {
        Self { monitors:(0..count).map(|_|Mutex::new(HeadMonitor::new(policy.clone(),enforce))).collect() }
    }
    pub fn observe(&self,index:usize,head:BlockRef,unix_seconds:u64) -> Result<()> {
        self.monitors.get(index).ok_or_else(||eyre!("RPC health endpoint index invalid"))?
            .lock().map_err(|_|eyre!("RPC health monitor unavailable"))?.observe(head,unix_seconds)
    }
}

pub struct RecoveryBackoff { policy: RecoveryPolicy, failures: u32 }
impl RecoveryBackoff {
    pub fn new(policy: RecoveryPolicy) -> Self { Self { policy,failures:0 } }
    pub fn reset(&mut self) { self.failures=0; }
    pub fn next_delay(&mut self) -> Duration {
        let factor=1u64 << self.failures.min(16);self.failures=self.failures.saturating_add(1);
        Duration::from_millis(self.policy.retry_initial_ms.saturating_mul(factor).min(self.policy.retry_max_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use barterlend_state::Commitment;
    use alloy::primitives::B256;
    fn head(number: u64,timestamp:u64) -> BlockRef { BlockRef { number,timestamp,hash:B256::from(uint_hash(number)),parent_hash:B256::ZERO,commitment:Commitment::Latest } }
    fn uint_hash(number:u64) -> [u8;32] { let mut b=[0;32];b[24..].copy_from_slice(&number.to_be_bytes());b }
    #[test]
    fn frozen_successful_rpc_does_not_refresh_progress_and_recovers_on_new_head() {
        let policy=RecoveryPolicy::default();let mut monitor=HeadMonitor::new(policy,true);let at=Instant::now();
        monitor.observe_at(head(1,1000),1000,at).unwrap();
        assert!(monitor.observe_at(head(1,1000),1031,at+Duration::from_secs(31)).is_err());
        monitor.observe_at(head(2,1032),1032,at+Duration::from_secs(32)).unwrap();
        assert!(monitor.observe_at(head(3,1000),1201,at+Duration::from_secs(33)).is_err());
    }
    #[test]
    fn local_fixed_chain_is_explicitly_exempt_and_backoff_is_bounded() {
        let policy=RecoveryPolicy::default();let mut monitor=HeadMonitor::new(policy.clone(),false);
        monitor.observe(head(1,1),1_800_000_000).unwrap();
        let mut backoff=RecoveryBackoff::new(policy.clone());
        for _ in 0..100 { assert!(backoff.next_delay()<=Duration::from_millis(policy.retry_max_ms)); }
        backoff.reset();assert_eq!(backoff.next_delay(),Duration::from_millis(policy.retry_initial_ms));
    }
}
