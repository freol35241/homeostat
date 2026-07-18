//! Supervision integration tests: each scenario spawns the real `homeostat`
//! binary on a fixture house and asserts on the bus.

mod common;

use std::time::{Duration, Instant};

use homeostat::bus::{Health, HealthStatus, LogEntry};
use zenoh::sample::SampleKind;

use common::{await_health, health_watch, process_alive, scan_health, Supervisor};

const STATE_KEY: &str = "home/state/testroom/fake_sensor/value";
const CRASH_KEY: &str = "home/cmd/testroom/fake_sensor/crash";

/// Reads a unit's log ring buffer over the bus (`?lines=N` when given),
/// decoding the single JSON reply into typed entries. No reply (unknown
/// unit, or a unit with no captured output yet) reads back as empty.
async fn get_log(session: &zenoh::Session, unit: &str, lines: Option<u32>) -> Vec<LogEntry> {
    let mut selector = homeostat::bus::log_key(unit);
    if let Some(n) = lines {
        selector.push_str(&format!("?lines={n}"));
    }
    let replies = session.get(&selector).await.expect("log get");
    let mut entries = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            entries = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("log entries parse");
        }
    }
    entries
}

/// Polls the log queryable until `pred` is satisfied or the deadline
/// passes; panics on timeout with the last-seen buffer for debugging.
async fn await_log<F>(session: &zenoh::Session, unit: &str, timeout: Duration, pred: F) -> Vec<LogEntry>
where
    F: Fn(&[LogEntry]) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let entries = get_log(session, unit, None).await;
        if pred(&entries) {
            return entries;
        }
        assert!(Instant::now() < deadline, "log condition not met in time: {entries:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn running(h: &Health) -> bool {
    h.status == HealthStatus::Running
}

/// (a) Spawn: the unit's liveliness token appears and fake state flows.
#[tokio::test(flavor = "multi_thread")]
async fn spawn_shows_liveliness_and_state_flows() {
    let mut sup = Supervisor::spawn("tests/fixture_house");
    let observer = sup.observer().await;

    let token_sub = observer
        .liveliness()
        .declare_subscriber("home/health/fake/alive")
        .history(true)
        .await
        .expect("liveliness subscriber");
    let token = tokio::time::timeout(Duration::from_secs(10), token_sub.recv_async())
        .await
        .expect("liveliness token within 10s")
        .expect("liveliness stream open");
    assert_eq!(token.kind(), SampleKind::Put);
    assert_eq!(token.key_expr().as_str(), "home/health/fake/alive");

    let state_sub = observer
        .declare_subscriber(STATE_KEY)
        .await
        .expect("state subscriber");
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), state_sub.recv_async())
            .await
            .expect("state sample within 10s")
            .expect("state stream open");
    }

    let mut watch = health_watch(&observer, "fake").await;
    let health = await_health(&mut watch, Duration::from_secs(10), running).await;
    assert!(health.pid.is_some());

    sup.shutdown();
}

/// (b) Induced crash: restart, with observable exponential backoff.
#[tokio::test(flavor = "multi_thread")]
async fn crash_restarts_with_exponential_backoff() {
    let mut sup = Supervisor::spawn("tests/fixture_house");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "fake").await;

    let mut backoffs: Vec<u64> = Vec::new();
    let mut seen_restarts = 0;
    while backoffs.len() < 3 {
        await_health(&mut watch, Duration::from_secs(10), running).await;
        // The crash command has no last-value storage behind it yet, so a
        // put can land before the fresh incarnation subscribes; resend
        // until a new backoff transition shows up.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let backoff = loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no backoff observed after repeated crash commands"
            );
            observer.put(CRASH_KEY, "crash").await.expect("crash put");
            let found = scan_health(&mut watch, Duration::from_millis(700), |h| {
                h.status == HealthStatus::Backoff && h.restarts > seen_restarts
            })
            .await;
            if let Some(h) = found {
                break h;
            }
        };
        seen_restarts = backoff.restarts;
        backoffs.push(backoff.backoff_ms.expect("backoff_ms present"));
    }
    assert_eq!(backoffs, vec![100, 200, 400], "exponential backoff delays");

    // And it actually came back after all that.
    await_health(&mut watch, Duration::from_secs(10), running).await;
    sup.shutdown();
}

/// (c) Crash loop: the circuit breaker opens at home/health/{unit}.
#[tokio::test(flavor = "multi_thread")]
async fn crash_loop_opens_circuit_breaker() {
    let mut sup = Supervisor::spawn("tests/fixture_house_crashloop");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "crasher").await;

    let open = await_health(&mut watch, Duration::from_secs(20), |h| {
        h.status == HealthStatus::Open
    })
    .await;
    assert_eq!(open.restarts, 4, "restarts before the breaker opened");
    assert_eq!(open.last_exit_code, Some(1));

    // The breaker holds: nothing but `open` on the health key afterwards.
    let relapse = scan_health(&mut watch, Duration::from_millis(1500), |h| {
        h.status != HealthStatus::Open
    })
    .await;
    assert!(relapse.is_none(), "unit restarted after the breaker opened");

    sup.shutdown();
}

/// (d) SIGTERM: graceful shutdown within grace, leaving no orphans.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_shuts_down_gracefully_without_orphans() {
    let mut sup = Supervisor::spawn("tests/fixture_house");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "fake").await;
    let health = await_health(&mut watch, Duration::from_secs(10), running).await;
    let adapter_pid = health.pid.expect("running unit has a pid");
    assert!(process_alive(adapter_pid), "adapter alive before shutdown");

    sup.signal(libc::SIGTERM);
    // shutdown_grace_s = 5 in the fixture; a graceful exit must fit inside
    // it with margin only for reaping and bus teardown.
    let code = sup.wait_exit(Duration::from_secs(7));
    assert_eq!(code, Some(0), "supervisor exit code");
    assert!(!process_alive(adapter_pid), "adapter must not outlive the supervisor");
}

/// SIGKILL on the supervisor must still not leak the unit (pdeathsig).
#[tokio::test(flavor = "multi_thread")]
async fn sigkill_leaves_no_orphans() {
    let mut sup = Supervisor::spawn("tests/fixture_house");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "fake").await;
    let health = await_health(&mut watch, Duration::from_secs(10), running).await;
    let adapter_pid = health.pid.expect("running unit has a pid");

    sup.signal(libc::SIGKILL);
    sup.wait_exit(Duration::from_secs(5));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while process_alive(adapter_pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "adapter survived SIGKILL of the supervisor"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// (f) Log capture: a unit's stdout/stderr are tagged by stream, ordered
/// within each stream, and truncatable from the tail via `?lines=N`.
#[tokio::test(flavor = "multi_thread")]
async fn log_capture_tags_and_orders_by_stream() {
    let mut sup = Supervisor::spawn("tests/fixture_house_logs");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "logger").await;
    await_health(&mut watch, Duration::from_secs(10), running).await;

    let entries = await_log(&observer, "logger", Duration::from_secs(10), |e| e.len() >= 8).await;
    assert_eq!(entries.len(), 8, "5 stdout + 3 stderr lines: {entries:?}");

    let stdout: Vec<&str> = entries
        .iter()
        .filter(|e| e.stream == "stdout")
        .map(|e| e.line.as_str())
        .collect();
    assert_eq!(
        stdout,
        vec!["stdout-line-0", "stdout-line-1", "stdout-line-2", "stdout-line-3", "stdout-line-4"],
        "stdout lines keep source order: {entries:?}"
    );
    let stderr: Vec<&str> = entries
        .iter()
        .filter(|e| e.stream == "stderr")
        .map(|e| e.line.as_str())
        .collect();
    assert_eq!(
        stderr,
        vec!["stderr-line-0", "stderr-line-1", "stderr-line-2"],
        "stderr lines keep source order: {entries:?}"
    );
    assert!(entries.iter().all(|e| e.ts_us > 0), "{entries:?}");

    // lines=N truncates to the tail of whatever order actually landed.
    let tail = get_log(&observer, "logger", Some(3)).await;
    assert_eq!(tail.len(), 3, "{tail:?}");
    assert_eq!(
        tail.iter().map(|e| &e.line).collect::<Vec<_>>(),
        entries[entries.len() - 3..].iter().map(|e| &e.line).collect::<Vec<_>>(),
        "lines=3 is the last 3 of the full buffer: {entries:?} vs {tail:?}"
    );

    // An unknown unit's log key gets no reply.
    let unknown = get_log(&observer, "no-such-unit", None).await;
    assert!(unknown.is_empty(), "{unknown:?}");

    sup.shutdown();
}

/// (g) Ring-buffer eviction: printing past the 500-line capacity drops the
/// oldest lines, keeping exactly the most recent 500.
#[tokio::test(flavor = "multi_thread")]
async fn log_capture_evicts_oldest_past_capacity() {
    let mut sup = Supervisor::spawn("tests/fixture_house_logs");
    let observer = sup.observer().await;
    let mut watch = health_watch(&observer, "flooder").await;
    await_health(&mut watch, Duration::from_secs(10), running).await;

    let entries =
        await_log(&observer, "flooder", Duration::from_secs(10), |e| e.len() >= 500).await;
    assert_eq!(entries.len(), 500, "capped at the ring buffer capacity: {} entries", entries.len());
    assert!(
        entries.iter().all(|e| e.stream == "stdout"),
        "flooder only writes stdout: {entries:?}"
    );
    assert_eq!(entries.first().unwrap().line, "stdout-line-100", "oldest 100 evicted");
    assert_eq!(entries.last().unwrap().line, "stdout-line-599", "newest line kept");

    sup.shutdown();
}
