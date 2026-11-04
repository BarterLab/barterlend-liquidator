//! Small read-only probe server. No configuration, keys or signing API is exposed.
use std::{net::SocketAddr, sync::{Arc, RwLock, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant}};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, task::JoinSet};
use color_eyre::eyre::Result;
use crate::diagnostics::HealthSnapshot;

type Health = Arc<RwLock<Option<HealthSnapshot>>>;
/// Monotonic progress of the control loop, independent of RPC availability.
pub struct Progress { started: Instant, last_ms: AtomicU64 }
impl Default for Progress { fn default()->Self {Self {started:Instant::now(),last_ms:AtomicU64::new(0)}} }
impl Progress {
    pub fn tick(&self) {self.last_ms.store(self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,Ordering::Relaxed);}
    fn live(&self)->bool {self.started.elapsed().as_millis().saturating_sub(u128::from(self.last_ms.load(Ordering::Relaxed)))<180_000}
}
pub struct HealthServer(tokio::task::JoinHandle<()>);
impl Drop for HealthServer { fn drop(&mut self) { self.0.abort(); } }

pub async fn start(address: Option<SocketAddr>, health: Health, progress: Arc<Progress>) -> Result<Option<HealthServer>> {
    let Some(address) = address else { return Ok(None); };
    let listener = TcpListener::bind(address).await?;
    Ok(Some(HealthServer(tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                result = listener.accept(), if connections.len() < 32 => {
                    match result {
                        Ok((stream, _)) => {
                            let health = health.clone();
                            let progress = progress.clone();
                            connections.spawn(async move {
                                let _ = tokio::time::timeout(Duration::from_secs(2), serve(stream, health, progress)).await;
                            });
                        },
                        Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                },
                _ = connections.join_next(), if !connections.is_empty() => {},
            }
        }
    }))))
}

async fn serve(mut stream: TcpStream, health: Health, progress: Arc<Progress>) -> std::io::Result<()> {
    let mut request = [0u8; 4096];
    let mut used = 0;
    loop {
        let count = stream.read(&mut request[used..]).await?;
        if count == 0 { return Ok(()); }
        used += count;
        if request[..used].windows(4).any(|v| v == b"\r\n\r\n") { break; }
        if used == request.len() { return Ok(()); }
    }
    let first = std::str::from_utf8(&request[..used]).unwrap_or("").lines().next().unwrap_or("");
    let mut parts = first.split_ascii_whitespace();
    let method = parts.next(); let path = parts.next();
    let snapshot = health.read().ok().and_then(|h| h.clone()).map(HealthSnapshot::current);
    let ready = snapshot.as_ref().is_some_and(|h| h.status == "ready");
    let (status, content_type, body) = match (method, path) {
        // RPC outages affect readiness, never cause a liveness restart storm.
        (Some("GET"), Some("/live")) => (if progress.live() {200} else {503}, "text/plain", if progress.live() {"ok\n"} else {"control loop stalled\n"}.into()),
        (Some("GET"), Some("/ready")) => (if ready { 200 } else { 503 }, "application/json",
            serde_json::to_string(&snapshot).unwrap_or_else(|_| "null".into())),
        (Some("GET"), Some("/metrics")) => (200, "text/plain; version=0.0.4", metrics(snapshot.as_ref(), ready)),
        _ => (404, "text/plain", "not found\n".into()),
    };
    let response = format!("HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else if status == 503 { "Service Unavailable" } else { "Not Found" }, body.len());
    stream.write_all(response.as_bytes()).await
}

fn metrics(snapshot: Option<&HealthSnapshot>, ready: bool) -> String {
    let mut result = format!("barterlend_ready {}\n", u8::from(ready));
    if let Some(h) = snapshot {
        result.push_str(&format!("barterlend_queue_length {}\nbarterlend_finalized_liquidations_total {}\nbarterlend_dry_run_plans_total {}\nbarterlend_skips_total {}\nbarterlend_log_dropped_total {}\nbarterlend_tx_pending {}\n",
            h.queue_len, h.counters.finalized_liquidations, h.counters.dry_run_plans, h.counters.skips, h.delivery.log_dropped, u8::from(h.tx.nonce.is_some())));
        result.push_str(&format!("barterlend_health_frame_age_seconds {}\n",h.frame_age_ms as f64/1000.0));
        if let Some(age)=h.head_age_ms {result.push_str(&format!("barterlend_head_observation_age_seconds {}\n",age as f64/1000.0));}
        for pool in &h.pools {
            if let Some(age)=pool.state_age_ms {result.push_str(&format!("barterlend_state_age_seconds{{pool=\"{}\"}} {}\n",pool.pool,age as f64/1000.0));}
            result.push_str(&format!("barterlend_positions{{pool=\"{}\"}} {}\nbarterlend_block_lag{{pool=\"{}\"}} {}\n",
                pool.pool, pool.positions_total.unwrap_or(0), pool.pool, pool.block_lag.unwrap_or(0)));
        }
    }
    result
}

pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)] {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { signal = tokio::signal::ctrl_c() => signal, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))] { tokio::signal::ctrl_c().await }
}

/// Never include a hot signing key in a process core dump.
pub fn disable_core_dumps() -> std::io::Result<()> {
    #[cfg(unix)] {
        let limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: setrlimit reads a valid rlimit; no memory ownership is transferred.
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 { return Err(std::io::Error::last_os_error()); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn get(path:&str, health:Health)->reqwest::Response {
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        tokio::spawn(async move {let(stream,_)=listener.accept().await.unwrap();serve(stream,health,Arc::new(Progress::default())).await.unwrap();});
        reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build().unwrap()
            .get(format!("http://{address}{path}")).send().await.unwrap()
    }
    #[tokio::test]
    async fn probes_are_read_only_and_never_report_unobserved_state_ready() {
        let health=Arc::new(RwLock::new(None));
        assert_eq!(get("/live",health.clone()).await.status(),200);
        assert_eq!(get("/ready",health.clone()).await.status(),503);
        assert_eq!(get("/config",health.clone()).await.status(),404);
        assert_eq!(get("/metrics",health.clone()).await.text().await.unwrap(),"barterlend_ready 0\n");
        assert!(health.read().unwrap().is_none());
    }
    #[test]
    fn progress_watchdog_detects_a_stall_and_recovers_without_wall_clock() {
        let progress=Progress {started:Instant::now()-Duration::from_secs(181),last_ms:AtomicU64::new(0)};
        assert!(!progress.live());progress.tick();assert!(progress.live());
    }
}
