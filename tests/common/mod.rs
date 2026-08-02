//! Harness for the supervision integration tests: spawns the real
//! `homeostat` binary on a fixture house with an isolated bus endpoint, and
//! opens an observer session to assert on bus traffic.

use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::{self, Health, HealthStatus};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::Sample;

pub struct Supervisor {
    child: Child,
    pub endpoint: String,
    stderr_path: PathBuf,
}

impl Supervisor {
    /// Spawns `homeostat up <fixture> --listen <fresh port>` with the
    /// fake_adapter binary's directory on PATH, and waits until the bus
    /// endpoint accepts connections.
    #[allow(dead_code)] // each test binary uses its own subset of the harness
    pub fn spawn(fixture: &str) -> Self {
        Self::spawn_with_env(fixture, &[])
    }

    /// Like `spawn`, with extra environment variables that the supervisor
    /// (and therefore its units) inherit. Ephemeral ports are handed out
    /// racily (the probe listener closes before the supervisor binds), so
    /// a supervisor that dies before listening is retried on a new port.
    pub fn spawn_with_env(fixture: &str, envs: &[(&str, &str)]) -> Self {
        Self::spawn_at(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture), envs)
    }

    /// Like `spawn_with_env`, on an absolute house path (temp-dir copies
    /// that tests edit between plan and apply).
    #[allow(dead_code)] // each test binary uses its own subset of the harness
    pub fn spawn_at(house: &std::path::Path, envs: &[(&str, &str)]) -> Self {
        for _ in 0..5 {
            match Self::try_spawn(house, envs) {
                Some(sup) => return sup,
                None => continue,
            }
        }
        panic!("supervisor kept exiting before listening");
    }

    fn try_spawn(house: &std::path::Path, envs: &[(&str, &str)]) -> Option<Self> {
        let port = free_port();
        let endpoint = format!("tcp/127.0.0.1:{port}");
        let fake_adapter_dir = PathBuf::from(env!("CARGO_BIN_EXE_fake_adapter"))
            .parent()
            .expect("bin dir")
            .to_path_buf();
        let path = format!(
            "{}:{}",
            fake_adapter_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let stderr_path =
            std::env::temp_dir().join(format!("homeostat-test-sup-{port}.stderr"));
        let stderr = std::fs::File::create(&stderr_path).expect("create stderr capture");
        let mut command = Command::new(env!("CARGO_BIN_EXE_homeostat"));
        command
            .args(["up", house.to_str().expect("utf-8 path"), "--listen", &endpoint])
            .env("PATH", path)
            .stdout(Stdio::null())
            .stderr(stderr);
        for (key, value) in envs {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn supervisor");
        let mut sup = Self { child, endpoint, stderr_path };
        if sup.await_listening() {
            Some(sup)
        } else {
            None
        }
    }

    /// True once the endpoint accepts connections; false if the supervisor
    /// exited first (e.g. it lost a bind race for the probed port).
    fn await_listening(&mut self) -> bool {
        let addr = self.endpoint.trim_start_matches("tcp/").to_string();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if TcpStream::connect(&addr).is_ok() {
                return true;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                eprintln!(
                    "supervisor exited before listening ({status}): {}",
                    self.stderr(),
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("supervisor never listened on {}", self.endpoint);
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    pub fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(self.pid(), signal);
        }
    }

    /// Waits for the supervisor process to exit, returning its exit code.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("supervisor did not exit within {timeout:?}");
    }

    /// Opens a client session on the supervisor's bus. A single connect
    /// attempt can lose to a loaded host (the TCP probe in `spawn` proves
    /// the endpoint listens, but a zenoh open right after may still time
    /// out), so failures retry within a deadline.
    pub async fn observer(&self) -> zenoh::Session {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match zenoh::open(bus::connect_config(&self.endpoint)).await {
                Ok(session) => return session,
                Err(err) if tokio::time::Instant::now() < deadline => {
                    eprintln!("observer session failed ({err}), retrying");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(err) => panic!(
                    "observer session failed: {err}; supervisor stderr: {}",
                    self.stderr()
                ),
            }
        }
    }

    /// Graceful teardown used by tests that already asserted what they
    /// needed: SIGTERM, then require a clean exit.
    pub fn shutdown(&mut self) {
        self.signal(libc::SIGTERM);
        let code = self.wait_exit(Duration::from_secs(10));
        assert_eq!(code, Some(0), "supervisor exit code");
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// True while a process exists and is not a zombie.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn process_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Third field after the parenthesized comm is the state.
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.split_whitespace().next());
    state != Some("Z")
}

pub type HealthSub = Subscriber<FifoChannelHandler<Sample>>;

/// A health watch: live subscription plus the current value fetched from
/// the supervisor's health queryable. Health is published on transitions
/// only; the get covers everything before the subscription, the subscriber
/// everything after.
pub struct HealthWatch {
    sub: HealthSub,
    pending: std::collections::VecDeque<Health>,
}

pub async fn health_watch(session: &zenoh::Session, unit: &str) -> HealthWatch {
    let sub = session
        .declare_subscriber(bus::health_key(unit))
        .await
        .expect("health subscriber");
    let replies = session
        .get(bus::health_key(unit))
        .await
        .expect("health get");
    let mut pending = std::collections::VecDeque::new();
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            let health: Health = serde_json::from_slice(&sample.payload().to_bytes())
                .expect("health payload parses");
            pending.push_back(health);
        }
    }
    HealthWatch { sub, pending }
}

/// Reads health states until one satisfies `pred`; panics on timeout.
pub async fn await_health<F>(watch: &mut HealthWatch, timeout: Duration, pred: F) -> Health
where
    F: Fn(&Health) -> bool,
{
    scan_health(watch, timeout, pred)
        .await
        .expect("health condition not met in time")
}

/// Like `await_health`, but a timeout returns None instead of panicking.
pub async fn scan_health<F>(watch: &mut HealthWatch, timeout: Duration, pred: F) -> Option<Health>
where
    F: Fn(&Health) -> bool,
{
    while let Some(health) = watch.pending.pop_front() {
        if pred(&health) {
            return Some(health);
        }
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        match tokio::time::timeout(deadline - now, watch.sub.recv_async()).await {
            Err(_) => return None,
            Ok(Err(_)) => panic!("health subscriber closed"),
            Ok(Ok(sample)) => {
                let health: Health = serde_json::from_slice(&sample.payload().to_bytes())
                    .expect("health payload parses");
                if pred(&health) {
                    return Some(health);
                }
            }
        }
    }
}

/// A mosquitto broker on a free port, killed on drop.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub struct Mosquitto {
    child: Child,
    pub port: u16,
    conf: PathBuf,
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
impl Mosquitto {
    pub fn spawn() -> Self {
        let port = free_port();
        let conf = std::env::temp_dir().join(format!("homeostat-mqtt-{port}.conf"));
        std::fs::write(&conf, format!("listener {port} 127.0.0.1\nallow_anonymous true\n"))
            .expect("write mosquitto config");
        // Debian puts mosquitto in /usr/sbin, which is not always on PATH.
        let child = ["mosquitto", "/usr/sbin/mosquitto"]
            .iter()
            .find_map(|bin| {
                Command::new(bin)
                    .args(["-c", conf.to_str().expect("utf-8 conf path")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            })
            .expect("spawn mosquitto (is it installed?)");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "mosquitto never listened on {port}");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port, conf }
    }
}

impl Drop for Mosquitto {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.conf);
    }
}

/// An MQTT test client: publishes are acknowledged (QoS 1) before returning,
/// incoming messages are buffered so acks and messages can interleave.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub struct Mqtt {
    client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    inbox: VecDeque<(String, Vec<u8>)>,
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
impl Mqtt {
    pub async fn connect(port: u16, id: &str) -> Self {
        let mut opts = MqttOptions::new(id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(event) = eventloop.poll().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let mut mqtt = Self { client, events, inbox: VecDeque::new() };
        mqtt.await_event(|i| matches!(i, Incoming::ConnAck(_))).await;
        mqtt
    }

    pub async fn subscribe(&mut self, topic: &str) {
        self.client
            .subscribe(topic, QoS::AtLeastOnce)
            .await
            .expect("mqtt subscribe");
        self.await_event(|i| matches!(i, Incoming::SubAck(_))).await;
    }

    pub async fn publish(&mut self, topic: &str, payload: &str) {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .await
            .expect("mqtt publish");
        self.await_event(|i| matches!(i, Incoming::PubAck(_))).await;
    }

    /// Reads events until `pred` matches, buffering message publishes.
    async fn await_event<F: Fn(&Incoming) -> bool>(&mut self, pred: F) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .expect("mqtt event within 10s")
                .expect("mqtt event loop alive");
            match event {
                Event::Incoming(Incoming::Publish(p)) => {
                    self.inbox.push_back((p.topic.clone(), p.payload.to_vec()));
                }
                Event::Incoming(incoming) if pred(&incoming) => return,
                _ => {}
            }
        }
    }

    /// Next subscribed message, or None if the timeout elapses first.
    pub async fn next_message(&mut self, timeout: Duration) -> Option<(String, Vec<u8>)> {
        if let Some(msg) = self.inbox.pop_front() {
            return Some(msg);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .ok()?
                .expect("mqtt event loop alive");
            if let Event::Incoming(Incoming::Publish(p)) = event {
                return Some((p.topic.clone(), p.payload.to_vec()));
            }
        }
    }
}

pub type StateSub = Subscriber<FifoChannelHandler<Sample>>;

/// Collects state samples until every `expected` (key, value) has appeared.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn expect_states(sub: &StateSub, expected: &[(&str, Value)]) {
    let mut seen: HashMap<String, Value> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while expected
        .iter()
        .any(|(key, value)| seen.get(*key) != Some(value))
    {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("missing state keys; saw {seen:?}"))
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        seen.insert(sample.key_expr().as_str().to_string(), value);
    }
}

/// Waits for a key to carry `expected`.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn expect_state(sub: &StateSub, expected: Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no value {expected} within 20s"))
            .expect("state stream open");
        let value: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("state payload is JSON");
        if value == expected {
            return;
        }
    }
}

/// Polls the core state mirror until `key` holds `expected` — the
/// late-joiner read path. Publishes that predate a test's subscriber
/// (connect-time availability, first states) are only observable here.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn await_mirror(observer: &zenoh::Session, key: &str, expected: &serde_json::Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let replies = observer.get(key).await.expect("mirror get");
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                if let Ok(value) =
                    serde_json::from_slice::<serde_json::Value>(&sample.payload().to_bytes())
                {
                    if &value == expected {
                        return;
                    }
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mirror never held {key} = {expected}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Reads health events until one matches the expected drop reason.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn expect_drop_event(sub: &StateSub, reason: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no \"{reason}\" health event within 20s"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("health event is JSON");
        assert_eq!(event["kind"], "drop", "unexpected event kind: {event}");
        if event["reason"] == reason {
            return event;
        }
    }
}

/// Reads health events until one matches the expected kind — degraded
/// conditions publish kind = condition (the backend-outage precedent),
/// unlike dropped-input events.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn expect_event_kind(sub: &StateSub, kind: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sample = tokio::time::timeout_at(deadline, sub.recv_async())
            .await
            .unwrap_or_else(|_| panic!("no \"{kind}\" health event within 20s"))
            .expect("event stream open");
        let event: Value = serde_json::from_slice(&sample.payload().to_bytes())
            .expect("health event is JSON");
        if event["kind"] == kind {
            return;
        }
    }
}

pub type Publisher = zenoh::pubsub::Publisher<'static>;

/// Declares a publisher and waits until a subscriber matches it, so
/// nothing this publisher puts is ever write-side filtered.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn matched_publisher(session: &zenoh::Session, key: &str) -> Publisher {
    let publisher = session
        .declare_publisher(key.to_string())
        .await
        .expect("publisher");
    await_matching(&publisher).await;
    publisher
}

/// Waits until a subscriber matches the publisher.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn await_matching(publisher: &Publisher) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = publisher.matching_status().await.expect("matching status");
        if status.matching() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no subscriber matched {}",
            publisher.key_expr()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Writes a parameter through the core's query-with-payload write path.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn config_write(
    session: &zenoh::Session,
    key: &str,
    value: Value,
) -> Result<Value, String> {
    let replies = session
        .get(key)
        .payload(value.to_string())
        .await
        .expect("config write query");
    let reply = replies.recv_async().await.expect("config write reply");
    match reply.result() {
        Ok(sample) => {
            Ok(serde_json::from_slice(&sample.payload().to_bytes()).expect("ok reply is JSON"))
        }
        Err(err) => Err(String::from_utf8_lossy(&err.payload().to_bytes()).to_string()),
    }
}

/// Reads a concrete key from a core queryable, decoding JSON.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn cache_read(session: &zenoh::Session, key: &str) -> Option<Value> {
    let replies = session.get(key).await.expect("cache read query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Some(
                serde_json::from_slice(&sample.payload().to_bytes()).expect("reply is JSON"),
            );
        }
    }
    None
}

/// Reads a concrete key from a core queryable as a raw string (meta values
/// like hashes and commits are not JSON).
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn meta_read(session: &zenoh::Session, key: &str) -> Option<String> {
    let replies = session.get(key).await.expect("meta read query");
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Some(String::from_utf8_lossy(&sample.payload().to_bytes()).to_string());
        }
    }
    None
}

/// The unit's current pid per the health queryable; panics unless running.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn running_pid(session: &zenoh::Session, unit: &str) -> u64 {
    let health = cache_read(session, &bus::health_key(unit))
        .await
        .unwrap_or_else(|| panic!("no health served for {unit}"));
    assert_eq!(health["status"], json!("running"), "{unit} health: {health}");
    health["pid"].as_u64().expect("running unit has a pid")
}

/// Waits until both fixture units are running and returns (probe pid,
/// reflector pid). Generous timeout: the first run resolves probe's uv env.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub async fn await_base_units(session: &zenoh::Session) -> (u64, u64) {
    let mut probe = health_watch(session, "probe").await;
    await_health(&mut probe, Duration::from_secs(120), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let mut reflector = health_watch(session, "reflector").await;
    await_health(&mut reflector, Duration::from_secs(30), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    (
        running_pid(session, "probe").await,
        running_pid(session, "reflector").await,
    )
}

/// A fresh editable copy of the `fixture` house (repo-relative path) in a
/// temp dir named after `tag`.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn temp_house(fixture: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("homeostat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture), &dir);
    dir
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dir");
    for entry in std::fs::read_dir(src).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn git(house: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(house)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn git_init_commit(house: &Path) -> String {
    git(house, &["init", "-q", "-b", "main"]);
    git_commit_all(house, "initial")
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn git_commit_all(house: &Path, message: &str) -> String {
    git(house, &["add", "-A"]);
    git(house, &["commit", "-qm", message]);
    git(house, &["rev-parse", "HEAD"])
}

/// Runs the homeostat CLI, returning its output.
#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_homeostat"))
        .args(args)
        .env_remove(bus::ENV_BUS)
        .output()
        .expect("run homeostat CLI")
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[allow(dead_code)] // each test binary uses its own subset of the harness
pub fn assert_cli_ok(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}
