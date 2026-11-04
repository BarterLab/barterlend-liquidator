//! Bounded best-effort I/O. No writer runs on the financial actor or Tokio workers.
use std::{io::{self, Write},
    sync::{Arc, OnceLock, atomic::{AtomicU64, Ordering}, mpsc}, time::Duration};
use serde::{Deserialize, Serialize};
use tracing_subscriber::{fmt::{format::FmtSpan, time::UtcTime, MakeWriter}, util::SubscriberInitExt};

const LOG_CAPACITY: usize = 128;
const LOG_BYTES: usize = 128 * 1024;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeliveryStats {
    pub log_pending: u64, pub log_dropped: u64, pub log_write_errors: u64,
}
#[derive(Default)]
struct LogCounters { pending: AtomicU64, dropped: AtomicU64, errors: AtomicU64 }
enum LogMessage { Record { bytes: Vec<u8>, error: bool }, Barrier(tokio::sync::oneshot::Sender<()>) }
struct Logger { sender: mpsc::SyncSender<LogMessage>, counters: Arc<LogCounters> }
impl Logger {
    fn start(capacity: usize, mut write: impl FnMut(&[u8],bool)->io::Result<()> + Send + 'static) -> io::Result<Self> {
        let (sender,receiver)=mpsc::sync_channel(capacity);
        let counters=Arc::new(LogCounters::default());let stats=counters.clone();
        std::thread::Builder::new().name("diagnostic-logs".into()).spawn(move || {
            while let Ok(message)=receiver.recv() {
                match message {
                    LogMessage::Record { bytes,error } => {
                        if write(&bytes,error).is_err() {stats.errors.fetch_add(1,Ordering::Relaxed);}
                        stats.pending.fetch_sub(1,Ordering::Relaxed);
                    },
                    LogMessage::Barrier(done)=>{let _=done.send(());},
                }
            }
        })?;
        Ok(Self {sender,counters})
    }
    fn offer(&self, bytes: Vec<u8>, error: bool) {
        self.counters.pending.fetch_add(1,Ordering::Relaxed);
        if self.sender.try_send(LogMessage::Record {bytes,error}).is_err() {
            self.counters.pending.fetch_sub(1,Ordering::Relaxed);
            self.counters.dropped.fetch_add(1,Ordering::Relaxed);
        }
    }
}
static LOGGER: OnceLock<Option<Logger>>=OnceLock::new();
static START_FAILURES: AtomicU64=AtomicU64::new(0);
fn logger() -> Option<&'static Logger> {
    LOGGER.get_or_init(|| Logger::start(LOG_CAPACITY,|bytes,error| {
        if error {io::stderr().lock().write_all(bytes)} else {io::stdout().lock().write_all(bytes)}
    }).ok()).as_ref()
}
struct Limited { bytes: Vec<u8>, cap: usize, failed: bool }
impl Write for Limited {
    fn write(&mut self, bytes:&[u8])->io::Result<usize> {
        if self.failed || bytes.len()>self.cap.saturating_sub(self.bytes.len()) {self.failed=true;return Err(io::Error::other("diagnostic record too large"));}
        let needed=self.bytes.len()+bytes.len();
        if needed>self.bytes.capacity() {self.bytes.reserve_exact(needed.max(self.bytes.capacity().saturating_mul(2)).min(self.cap)-self.bytes.len());}
        self.bytes.extend_from_slice(bytes);Ok(bytes.len())
    }
    fn flush(&mut self)->io::Result<()> {Ok(())}
}
/// A complete tracing record is enqueued on drop. Queue overflow drops logs,
/// never financial work. No file sink or unbounded buffering is used.
struct ConsoleWriter(Limited);
impl Write for ConsoleWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.0.write(bytes) }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
impl Drop for ConsoleWriter {
    fn drop(&mut self) {
        if self.0.failed { START_FAILURES.fetch_add(1, Ordering::Relaxed); return; }
        if let Some(logger) = logger() { logger.offer(std::mem::take(&mut self.0.bytes), false); }
        else { START_FAILURES.fetch_add(1, Ordering::Relaxed); }
    }
}
// Console text envelope: UTC RFC3339, level, target, fields.
fn console_subscriber<W>(filter: tracing_subscriber::EnvFilter, writer: W) -> impl tracing::Subscriber + Send + Sync
where W: for<'a> MakeWriter<'a> + Send + Sync + 'static {
    tracing_subscriber::fmt().with_env_filter(filter)
        .with_timer(UtcTime::rfc_3339()).with_span_events(FmtSpan::CLOSE).with_ansi(false)
        .with_writer(writer).finish()
}
static TRACING: OnceLock<()> = OnceLock::new();
pub fn init() {
    TRACING.get_or_init(|| {
        // Restrict filtering to our own target: third-party HTTP debug logging
        // must not expose credential-bearing URLs or request bodies.
        let level = std::env::var("LOG_FILTER").unwrap_or_else(|_| "info".into());
        let level = match level.as_str() { "error" | "warn" | "info" | "debug" | "trace" => level, _ => "info".into() };
        let filter = tracing_subscriber::EnvFilter::new(format!("off,barterlend_liquidator={level}"));
        let _ = console_subscriber(filter,
            || ConsoleWriter(Limited { bytes: Vec::with_capacity(1024), cap: LOG_BYTES, failed: false })).try_init();
    });
}
pub(crate) fn emit(value: &serde_json::Value, error: bool) {
    init();
    let event = value.get("event").and_then(|v| v.as_str()).unwrap_or("event");
    let expected_skip = event == "diagnostic_error"
        && value.get("code").and_then(|v| v.as_str()) == Some("position_preparation_failed");
    if expected_skip || matches!(event, "skipped" | "candidate_skipped" | "position_skipped" | "state_discovery_progress" | "rpc_endpoint_unavailable") {
        tracing::debug!(event, details = %value);
    } else if error { tracing::error!(event, details = %value); }
    else { tracing::info!(event, details = %value); }
}
/// Bounded shutdown grace; never waits forever for a blocked log sink.
pub async fn flush_logs(timeout:Duration)->bool {
    let Some(logger)=LOGGER.get().and_then(Option::as_ref) else {return true;};
    let (done,wait)=tokio::sync::oneshot::channel();
    if logger.sender.try_send(LogMessage::Barrier(done)).is_err() {return false;}
    matches!(tokio::time::timeout(timeout,wait).await,Ok(Ok(())))
}

pub(crate) fn stats() -> DeliveryStats {
    let mut value = DeliveryStats { log_dropped: START_FAILURES.load(Ordering::Relaxed), ..Default::default() };
    if let Some(logger) = LOGGER.get().and_then(Option::as_ref) {
        value.log_pending = logger.counters.pending.load(Ordering::Relaxed);
        value.log_dropped = value.log_dropped.saturating_add(logger.counters.dropped.load(Ordering::Relaxed));
        value.log_write_errors = logger.counters.errors.load(Ordering::Relaxed);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.0.lock().unwrap().extend_from_slice(bytes); Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    fn sample_events() {
        let details = serde_json::json!({"event":"config_valid","text":"quote \" and newline\nend"});
        tracing::info!(target:"barterlend_liquidator::format_test",event="config_valid",details=%details);
        tracing::error!(target:"barterlend_liquidator::format_test",event="diagnostic_error",code="command_failed");
    }
    #[test]
    fn console_format_matches_reference_text_envelope() {
        let actual = Capture::default(); let writer = actual.clone();
        tracing::subscriber::with_default(console_subscriber(tracing_subscriber::EnvFilter::new("trace"),move || writer.clone()),sample_events);
        let expected = Capture::default(); let writer = expected.clone();
        // Independent tracing-layer setup used as the format reference.
        let reference = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_timer(UtcTime::rfc_3339())
                .with_span_events(FmtSpan::CLOSE).with_ansi(false).with_writer(move || writer.clone()))
            .with(tracing_subscriber::EnvFilter::new("trace"));
        tracing::subscriber::with_default(reference,sample_events);
        let text = String::from_utf8(actual.0.lock().unwrap().clone()).unwrap();
        let reference = String::from_utf8(expected.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(),2); assert!(!text.contains('\u{1b}'));
        for (actual,expected) in text.lines().zip(reference.lines()) {
            let (timestamp,body) = actual.split_once(' ').unwrap();
            assert!(timestamp.ends_with('Z') && timestamp.as_bytes().get(10)==Some(&b'T'));
            assert_eq!(body,expected.split_once(' ').unwrap().1);
        }
        let details:serde_json::Value=serde_json::from_str(text.lines().next().unwrap().split_once(" details=").unwrap().1).unwrap();
        assert_eq!(details["text"],"quote \" and newline\nend");
    }

    #[test]
    fn oversized_record_is_discarded_whole() {
        let mut writer=ConsoleWriter(Limited { bytes:Vec::new(),cap:4,failed:false });
        writer.write_all(b"abc").unwrap();
        assert!(writer.write_all(b"def").is_err());
        assert!(writer.write_all(b"\n").is_err());
        assert!(writer.0.failed);
    }
    #[tokio::test(flavor="current_thread")]
    async fn blocked_sink_keeps_runtime_responsive_and_queue_bounded() {
        let (entered,wait_entered)=mpsc::channel();let (release,wait_release)=mpsc::channel();
        let (exited,wait_exited)=mpsc::channel();let mut first=true;
        let logger=Logger::start(2,move |_,_| {
            if first {first=false;entered.send(()).unwrap();wait_release.recv().unwrap();exited.send(()).unwrap();}Ok(())
        }).unwrap();
        logger.offer(vec![1],false);wait_entered.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..1000 {logger.offer(vec![1],false);}
        assert!(logger.counters.pending.load(Ordering::Relaxed)<=3);
        assert_eq!(logger.counters.dropped.load(Ordering::Relaxed),998);
        tokio::time::timeout(Duration::from_millis(200),tokio::time::sleep(Duration::from_millis(10))).await.unwrap();
        release.send(()).unwrap();wait_exited.recv_timeout(Duration::from_secs(2)).unwrap();
        let(done,wait)=tokio::sync::oneshot::channel();
        // Once released the finite queued records drain; no new writer is spawned.
        for _ in 0..100 {if logger.counters.pending.load(Ordering::Relaxed)==0 {break;}tokio::time::sleep(Duration::from_millis(2)).await;}
        logger.sender.try_send(LogMessage::Barrier(done)).unwrap();tokio::time::timeout(Duration::from_secs(2),wait).await.unwrap().unwrap();
    }
}
