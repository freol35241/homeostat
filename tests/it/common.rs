//! Harness for the supervision integration tests: spawns the real
//! `homeostat` binary on a fixture house with an isolated bus endpoint, and
//! opens an observer session to assert on bus traffic.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use homeostat::bus::{self, Health, HealthStatus};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use serde_json::{json, Value};
use zenoh::handlers::FifoChannelHandler;
use zenoh::pubsub::Subscriber;
use zenoh::sample::{Sample, SampleKind};

pub struct Supervisor {
    child: Child,
    pub endpoint: String,
    stderr_path: PathBuf,
    /// The spawn's own live edits file, removed on drop.
    live_edits: Option<PathBuf>,
}

impl Supervisor {
    /// Spawns `homeostat up <fixture> --listen <fresh port>` with the
    /// `fake_adapter` binary's directory on PATH, and waits until the bus
    /// endpoint accepts connections.
    pub fn spawn(fixture: &str) -> Self {
        Self::spawn_with_env(fixture, &[])
    }

    /// Like `spawn`, with extra environment variables that the supervisor
    /// (and therefore its units) inherit. Ephemeral ports are handed out
    /// racily (the probe listener closes before the supervisor binds), so
    /// a supervisor that dies before listening is retried on a new port.
    ///
    /// Tests share fixture houses, so each spawn saves its live edits to a
    /// file of its own instead of the house's `data/live-edits.json`.
    pub fn spawn_with_env(fixture: &str, envs: &[(&str, &str)]) -> Self {
        static SPAWNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let live_edits = std::env::temp_dir().join(format!(
            "homeostat-live-edits-{}-{}.json",
            std::process::id(),
            SPAWNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&live_edits);
        let path = live_edits.to_str().expect("utf-8 path").to_string();
        let mut envs = envs.to_vec();
        envs.push(("HOMEOSTAT_LIVE_EDITS", &path));
        let mut sup = Self::spawn_at(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture),
            &envs,
        );
        sup.live_edits = Some(live_edits);
        sup
    }

    /// Like `spawn_with_env`, on an absolute house path (temp-dir copies
    /// that tests edit between plan and apply).
    pub fn spawn_at(house: &std::path::Path, envs: &[(&str, &str)]) -> Self {
        for _ in 0..5 {
            if let Some(sup) = Self::try_spawn(house, envs) {
                return sup;
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
        let stderr_path = std::env::temp_dir().join(format!("homeostat-test-sup-{port}.stderr"));
        let stderr = std::fs::File::create(&stderr_path).expect("create stderr capture");
        let mut command = Command::new(env!("CARGO_BIN_EXE_homeostat"));
        command
            .args([
                "up",
                house.to_str().expect("utf-8 path"),
                "--listen",
                &endpoint,
            ])
            .env("PATH", path)
            .stdout(Stdio::null())
            .stderr(stderr);
        for (key, value) in envs {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn supervisor");
        let mut sup = Self {
            child,
            endpoint,
            stderr_path,
            live_edits: None,
        };
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

    /// The last `lines` lines the supervisor and its units printed, for a
    /// failure message: a unit that never becomes ready says why there.
    pub fn output_tail(&self, lines: usize) -> String {
        let all = self.stderr();
        let kept: Vec<&str> = all.lines().rev().take(lines).collect();
        kept.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("a pid fits i32")
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
        if let Some(live_edits) = &self.live_edits {
            let _ = std::fs::remove_file(live_edits);
        }
    }
}

/// A `uv run <script.py> [args]` fixture command, resolved the way the
/// supervisor resolves a unit's: the script's environment is materialised
/// by a process that exits, and what comes back is the interpreter
/// invoked on the script directly.
///
/// Fixtures must not be spawned through `uv run`. It stays alive as the
/// script's parent, so the tree is `test -> uv -> python3` and the handle
/// the fixture holds is uv's. `Child::kill` sends SIGKILL, which uv can
/// neither catch nor forward: uv dies, the interpreter is reparented to
/// init, and the fake server keeps listening for the life of the machine.
/// Exec'ing the interpreter makes the process the fixture holds the
/// process that has to die.
///
/// Falls back to the command as given when uv cannot resolve the script,
/// as `process::resolve` does. If that fixture then fails to start, the
/// test fails visibly; a leaked process would go unnoticed.
pub async fn fixture_command(command: &str) -> (String, Vec<String>) {
    let resolved =
        homeostat::supervisor::process::resolve(command, Path::new(env!("CARGO_MANIFEST_DIR")))
            .await;
    let mut parts = resolved.split_whitespace().map(str::to_string);
    let program = parts.next().expect("a non-empty command");
    (program, parts.collect())
}

/// A free port no earlier call in this test process returned. The kernel
/// readily hands a just-closed port out again, and the tests run
/// concurrently. Two tests given the same port each start a broker or a
/// supervisor on it, and the loser's fails to bind. Its spawn only checks
/// that something accepts on the port, so it carries on against the
/// winner's, and the test then asserts on another test's house. Unique
/// within the process is enough: every suite runs in this one binary.
pub fn free_port() -> u16 {
    static HANDED_OUT: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    let mut handed_out = HANDED_OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind ephemeral port")
            .local_addr()
            .expect("local addr")
            .port();
        if handed_out.insert(port) {
            return port;
        }
    }
}

/// True while a process exists and is not a zombie.
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
pub struct Mosquitto {
    child: Child,
    pub port: u16,
    conf: PathBuf,
}

/// Where a test broker's files go: Cargo's temporary directory for
/// integration tests, under `target/`. Ubuntu's `AppArmor` profile for
/// mosquitto (from 26.04) lets it read only `/etc/mosquitto`, so such a host
/// grants this one tree in the profile's local include (`CLAUDE.md`).
fn mosquitto_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("create the broker's directory");
    dir
}

impl Mosquitto {
    pub fn spawn() -> Self {
        Self::spawn_conf(|port, _| format!("listener {port} 127.0.0.1\nallow_anonymous true\n"))
    }

    /// A broker that requires the given credentials, with no anonymous
    /// fallback, so a unit that cannot present a password does not connect.
    pub fn spawn_with_auth(username: &str, password: &str) -> Self {
        let passwd = mosquitto_dir().join(format!(
            "homeostat-mqtt-passwd-{}-{username}",
            std::process::id()
        ));
        std::fs::write(&passwd, format!("{username}:{password}\n")).expect("write passwd");
        // mosquitto refuses a plaintext password file: hash it in place.
        let hashed = Command::new("mosquitto_passwd")
            .args(["-U", passwd.to_str().expect("utf-8 passwd path")])
            .status()
            .expect("run mosquitto_passwd")
            .success();
        assert!(hashed, "mosquitto_passwd failed");
        let passwd_arg = passwd.to_str().expect("utf-8 passwd path").to_string();
        Self::spawn_conf(move |port, _| {
            format!(
                "listener {port} 127.0.0.1\nallow_anonymous false\npassword_file {passwd_arg}\n"
            )
        })
    }

    /// Starts a broker on a free port and waits until it says it is
    /// running. A probed port can be taken by another test's broker or bus
    /// before this one binds it, and a TCP connect would then reach that
    /// listener instead, so readiness is the broker's own log line. A broker
    /// that lost the bind is retried on a fresh port.
    fn spawn_conf(body: impl Fn(u16, &Path) -> String) -> Self {
        for _ in 0..5 {
            let port = free_port();
            let conf = mosquitto_dir().join(format!("homeostat-mqtt-{port}.conf"));
            // Started as root, as in the CI container, mosquitto would drop
            // to its own user and could not read the password file the
            // test wrote. `user root` keeps it as the test's user; for any
            // other user the setting has no effect.
            let text = format!("user root\n{}", body(port, &conf));
            std::fs::write(&conf, text).expect("write mosquitto config");
            // The broker's own log, so a broker that never runs says why.
            let log = mosquitto_dir().join(format!("homeostat-mqtt-{port}.log"));
            // Debian puts mosquitto in /usr/sbin, which is not always on PATH.
            let mut child = ["mosquitto", "/usr/sbin/mosquitto"]
                .iter()
                .find_map(|bin| {
                    let out = std::fs::File::create(&log).expect("create mosquitto log");
                    let err = out.try_clone().expect("clone mosquitto log");
                    Command::new(bin)
                        .args(["-c", conf.to_str().expect("utf-8 conf path")])
                        .stdout(out)
                        .stderr(err)
                        .spawn()
                        .ok()
                })
                .expect("spawn mosquitto (is it installed?)");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let said = std::fs::read_to_string(&log).unwrap_or_default();
                let exited = child.try_wait().expect("poll mosquitto");
                if exited.is_none() && said.contains(" running") {
                    let _ = std::fs::remove_file(&log);
                    return Self { child, port, conf };
                }
                if exited.is_some() && said.contains("Address already in use") {
                    let _ = std::fs::remove_file(&log);
                    let _ = std::fs::remove_file(&conf);
                    break;
                }
                if exited.is_some() || Instant::now() >= deadline {
                    panic!(
                        "mosquitto never ran on {port} (exit: {exited:?}):\n{said}\n\
                         If it could not open its config, see CLAUDE.md on mosquitto's \
                         AppArmor profile."
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        panic!("mosquitto lost the bind race for five probed ports");
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
pub struct Mqtt {
    client: AsyncClient,
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    inbox: VecDeque<(String, Vec<u8>)>,
}

impl Mqtt {
    pub async fn connect_auth(port: u16, id: &str, username: &str, password: &str) -> Self {
        Self::connect_with(port, id, Some((username, password))).await
    }

    pub async fn connect(port: u16, id: &str) -> Self {
        Self::connect_with(port, id, None).await
    }

    async fn connect_with(port: u16, id: &str, auth: Option<(&str, &str)>) -> Self {
        let mut opts = MqttOptions::new(id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        if let Some((username, password)) = auth {
            opts.set_credentials(username, password);
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(event) = eventloop.poll().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let mut mqtt = Self {
            client,
            events,
            inbox: VecDeque::new(),
        };
        mqtt.await_event(|i| matches!(i, Incoming::ConnAck(_)))
            .await;
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
        self.publish_with(topic, payload, false).await;
    }

    pub async fn publish_retained(&mut self, topic: &str, payload: &str) {
        self.publish_with(topic, payload, true).await;
    }

    async fn publish_with(&mut self, topic: &str, payload: &str, retain: bool) {
        self.client
            .publish(topic, QoS::AtLeastOnce, retain, payload)
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

    /// Asserts that no message arrives for `window`; `what` names the
    /// message the test rules out.
    pub async fn expect_silence(&mut self, window: Duration, what: &str) {
        if let Some((topic, payload)) = self.next_message(window).await {
            panic!(
                "expected no {what}, got {topic}: {}",
                String::from_utf8_lossy(&payload)
            );
        }
    }
}

pub type StateSub = Subscriber<FifoChannelHandler<Sample>>;

/// How long a test listens to show that something was not sent. A
/// negative wait cannot end on a signal, so it is a fixed window. A test
/// whose window must span one of the unit's own intervals, such as a poll
/// cycle, uses that interval instead.
pub const SILENCE: Duration = Duration::from_millis(1500);

/// A sample off a subscription.
#[derive(Debug)]
pub struct Msg {
    pub key: String,
    /// The payload as JSON; None for a delete.
    pub json: Option<Value>,
}

impl Msg {
    /// The payload of a put; panics on a delete.
    pub fn value(&self) -> &Value {
        self.json
            .as_ref()
            .unwrap_or_else(|| panic!("{} was deleted, not put", self.key))
    }
}

/// The next sample within `timeout`, or None. Panics on a put whose
/// payload is not JSON: every key the suites subscribe to carries JSON.
pub async fn next_sample(sub: &StateSub, timeout: Duration) -> Option<Msg> {
    let sample = tokio::time::timeout(timeout, sub.recv_async())
        .await
        .ok()?
        .expect("subscription open");
    let key = sample.key_expr().as_str().to_string();
    let json = match sample.kind() {
        SampleKind::Delete => None,
        SampleKind::Put => Some(
            serde_json::from_slice(&sample.payload().to_bytes())
                .unwrap_or_else(|err| panic!("{key} payload is not JSON: {err}")),
        ),
    };
    Some(Msg { key, json })
}

/// Reads samples until `pred` accepts one and returns it. Panics after
/// `timeout`, naming `what` and the last sample it turned down.
pub async fn await_sample<F>(sub: &StateSub, timeout: Duration, what: &str, mut pred: F) -> Msg
where
    F: FnMut(&Msg) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(msg) = next_sample(sub, left).await else {
            panic!("no {what} within {timeout:?}; last sample {last:?}");
        };
        if pred(&msg) {
            return msg;
        }
        last = Some(msg);
    }
}

/// Asserts that `sub` delivers nothing for `window`; `what` names the
/// sample the test rules out.
pub async fn expect_silence(sub: &StateSub, window: Duration, what: &str) {
    if let Some(msg) = next_sample(sub, window).await {
        panic!("expected no {what}, got {msg:?}");
    }
}

/// Calls `probe` every 200 ms until it returns Some, and returns that.
/// Panics after `timeout`, naming `what`. This is how a test waits on
/// something it can only poll, such as the core mirror.
pub async fn eventually<T, F, Fut>(timeout: Duration, what: &str, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no {what} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Collects state samples until every `expected` (key, value) has appeared.
pub async fn expect_states(sub: &StateSub, expected: &[(&str, Value)]) {
    let mut seen: HashMap<String, Value> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while expected
        .iter()
        .any(|(key, value)| seen.get(*key) != Some(value))
    {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = next_sample(sub, left)
            .await
            .unwrap_or_else(|| panic!("missing state keys; saw {seen:?}"));
        if let Some(value) = msg.json {
            seen.insert(msg.key, value);
        }
    }
}

/// Waits for a key to carry `expected`.
pub async fn expect_state(sub: &StateSub, expected: Value) {
    let what = format!("value {expected}");
    await_sample(sub, Duration::from_secs(20), &what, |msg| {
        msg.json.as_ref() == Some(&expected)
    })
    .await;
}

/// Polls the core state mirror until `key` holds `expected`. This is the
/// late-joiner read path. Publishes that predate a test's subscriber
/// (connect-time availability, first states) are only observable here.
pub async fn await_mirror(observer: &zenoh::Session, key: &str, expected: &Value) {
    let what = format!("mirror value {key} = {expected}");
    eventually(Duration::from_mins(1), &what, || async {
        (cache_read(observer, key).await.as_ref() == Some(expected)).then_some(())
    })
    .await;
}

/// The next health event, whatever it is. Unlike `expect_drop_event` this
/// does not scan past events that do not match, which is what a test needs
/// when it asserts that some event was not emitted.
pub async fn next_event(sub: &StateSub) -> Value {
    next_sample(sub, Duration::from_secs(20))
        .await
        .expect("a health event within 20s")
        .value()
        .clone()
}

/// Reads health events until one matches the expected drop reason.
pub async fn expect_drop_event(sub: &StateSub, reason: &str) -> Value {
    let what = format!("\"{reason}\" health event");
    let msg = await_sample(sub, Duration::from_secs(20), &what, |msg| {
        let event = msg.value();
        assert_eq!(event["kind"], "drop", "unexpected event kind: {event}");
        event["reason"] == reason
    })
    .await;
    msg.value().clone()
}

/// Reads health events until one matches the expected kind. Degraded
/// conditions (a recorder backend outage, say) publish kind = condition,
/// unlike dropped-input events.
pub async fn expect_event_kind(sub: &StateSub, kind: &str) {
    let what = format!("\"{kind}\" health event");
    await_sample(sub, Duration::from_secs(20), &what, |msg| {
        msg.value()["kind"] == kind
    })
    .await;
}

pub type Publisher = zenoh::pubsub::Publisher<'static>;

/// Declares a publisher and waits until a subscriber matches it, so the
/// writer does not filter out anything this publisher puts.
pub async fn matched_publisher(session: &zenoh::Session, key: &str) -> Publisher {
    let publisher = session
        .declare_publisher(key.to_string())
        .await
        .expect("publisher");
    await_matching(&publisher).await;
    publisher
}

/// Waits until a subscriber matches the publisher.
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

/// The unit contract every supervised unit owes (docs/adapters.md, §2 and
/// §10; docs/design.md#the-unit-contract): once ready it is `running`
/// with a pid, and when the supervisor gets SIGTERM it exits cleanly
/// inside `shutdown_grace_s` and leaves no orphan. Each adapter suite
/// calls it once, so a new adapter's suite gets the check with one line.
pub async fn assert_unit_contract(sup: &mut Supervisor, observer: &zenoh::Session, unit: &str) {
    let mut watch = health_watch(observer, unit).await;
    let health = await_health(&mut watch, Duration::from_secs(10), |h| {
        h.status == HealthStatus::Running
    })
    .await;
    let pid = health.pid.expect("running unit has a pid");
    assert!(process_alive(pid), "{unit} alive before shutdown");

    sup.signal(libc::SIGTERM);
    // shutdown_grace_s = 5 in the fixtures; a graceful exit must fit inside
    // it with margin only for reaping and bus teardown.
    let code = sup.wait_exit(Duration::from_secs(7));
    assert_eq!(code, Some(0), "supervisor exit code");
    assert!(
        !process_alive(pid),
        "{unit} must not outlive the supervisor"
    );
}

/// The late-joiner read for a test: subscribe, then get, merge
/// (docs/design.md#the-last-value-mirror). A unit that reached
/// `running` may already have published its initial states before the
/// test's subscriber existed, because an adapter's device connection races
/// the liveliness token. A zenoh subscriber does not see past samples, so
/// a test that only waits on `sub` loses that race on a slow runner. This
/// drains the subscription and reads the core mirror until every expected
/// key carries its value. `sub` must already be declared (declaring it
/// after the mirror read would open the opposite gap).
pub async fn await_states(observer: &zenoh::Session, sub: &StateSub, expected: &[(&str, Value)]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut seen: HashMap<String, Value> = HashMap::new();
    let note = |sample: Sample, seen: &mut HashMap<String, Value>| {
        if let Ok(value) = serde_json::from_slice::<Value>(&sample.payload().to_bytes()) {
            seen.insert(sample.key_expr().as_str().to_string(), value);
        }
    };
    loop {
        while let Ok(Some(sample)) = sub.try_recv() {
            note(sample, &mut seen);
        }
        for (key, _) in expected {
            if !seen.contains_key(*key) {
                if let Some(value) = cache_read(observer, key).await {
                    seen.insert((*key).to_string(), value);
                }
            }
        }
        if expected
            .iter()
            .all(|(key, value)| seen.get(*key) == Some(value))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected:?} on the bus or in the mirror; saw {seen:?}"
        );
        // A subscription sample, or a re-read of the mirror after a beat.
        if let Ok(Ok(sample)) =
            tokio::time::timeout(Duration::from_millis(200), sub.recv_async()).await
        {
            note(sample, &mut seen);
        }
    }
}

/// A unit's discovery document as a late joiner reads it: polled from the
/// core mirror of `home/discovery/*` until `ready` accepts it (the record
/// count, say). The MCP surface reads it the same way, and it is the only
/// path a test can rely on when the publish may predate its subscriber.
pub async fn await_discovery<F>(observer: &zenoh::Session, unit: &str, ready: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let key = format!("home/discovery/{unit}");
    let what = format!("acceptable discovery document at {key}");
    eventually(Duration::from_secs(30), &what, || async {
        cache_read(observer, &key).await.filter(|doc| ready(doc))
    })
    .await
}

/// Reads a concrete key from a core queryable, decoding JSON.
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
pub async fn running_pid(session: &zenoh::Session, unit: &str) -> u64 {
    let health = cache_read(session, &bus::health_key(unit))
        .await
        .unwrap_or_else(|| panic!("no health served for {unit}"));
    assert_eq!(
        health["status"],
        json!("running"),
        "{unit} health: {health}"
    );
    health["pid"].as_u64().expect("running unit has a pid")
}

/// A fresh editable copy of the `fixture` house (repo-relative path) in a
/// temp dir named after `tag`.
///
/// Unit commands name their scripts relative to the fixture, as
/// `uv run ../../../adapters/x.py`, which resolves to nothing from the temp
/// dir. The copy's commands name them from the fixture's own directory.
pub fn temp_house(fixture: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("homeostat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture);
    copy_dir(&src, &dir);
    let units = std::fs::read_dir(dir.join("units")).expect("read the copy's units");
    for entry in units {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            let text = std::fs::read_to_string(&path).expect("read unit manifest");
            let text = text.replace("uv run ../", &format!("uv run {}/../", src.display()));
            std::fs::write(&path, text).expect("write unit manifest");
        }
    }
    dir
}

/// Replaces `from` with `to` in the house file at `rel`; panics if the
/// file does not contain `from`.
pub fn edit(house: &Path, rel: &str, from: &str, to: &str) {
    let path = house.join(rel);
    let text = std::fs::read_to_string(&path).expect("read house file");
    assert!(text.contains(from), "{rel} does not contain {from:?}");
    std::fs::write(&path, text.replace(from, to)).expect("write house file");
}

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

pub fn git_init_commit(house: &Path) -> String {
    git(house, &["init", "-q", "-b", "main"]);
    git_commit_all(house, "initial")
}

pub fn git_commit_all(house: &Path, message: &str) -> String {
    git(house, &["add", "-A"]);
    git(house, &["commit", "-qm", message]);
    git(house, &["rev-parse", "HEAD"])
}

/// Runs the homeostat CLI, returning its output.
pub fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_homeostat"))
        .args(args)
        .env_remove(bus::ENV_BUS)
        .output()
        .expect("run homeostat CLI")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

pub fn assert_cli_ok(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// Caps how many houses start at the same moment.
///
/// Every test spawns its own supervisor, and cargo runs one test per core.
/// Started all at once, the houses' units resolve and import together, and
/// on a two-core runner the slowest starves past its health deadline and
/// fails a test that nothing about it broke. A cap equal to the core count
/// would never bind, so it is half of it, and at least two.
///
/// `up` takes a permit, and so does a test that subscribes before its
/// units run. A few tests start a house and wait on it for something other
/// than a unit running, and go without one.
///
/// The permit covers start-up only. `up` holds it until the units under
/// test run, then drops it: the body of a test costs nothing to overlap,
/// and holding it longer would serialize the suite.
static STARTUP: std::sync::LazyLock<tokio::sync::Semaphore> = std::sync::LazyLock::new(|| {
    let cores = std::thread::available_parallelism().map_or(2, usize::from);
    tokio::sync::Semaphore::new((cores / 2).max(2))
});

/// A start-up permit, for a test that starts its house without `up`
/// because it must subscribe before the units run.
pub async fn startup_permit() -> tokio::sync::SemaphorePermit<'static> {
    STARTUP
        .acquire()
        .await
        .expect("startup semaphore never closes")
}

/// How long `up` waits for a unit to run. The first run of a unit resolves
/// its uv environment, which can take minutes on a cold cache.
pub const FIRST_RUN: Duration = Duration::from_mins(3);

/// Starts a house with `spawn` under a start-up permit and waits until each
/// of `units` runs. Returns the supervisor and an observer session on its
/// bus. A unit that does not run in time fails the test with the
/// supervisor's output, which says why.
pub async fn up(
    spawn: impl FnOnce() -> Supervisor,
    units: &[&str],
) -> (Supervisor, zenoh::Session) {
    let permit = startup_permit().await;
    let sup = spawn();
    let observer = sup.observer().await;
    for unit in units {
        let mut watch = health_watch(&observer, unit).await;
        if scan_health(&mut watch, FIRST_RUN, |h| h.status == HealthStatus::Running)
            .await
            .is_none()
        {
            panic!(
                "{unit} did not run within {FIRST_RUN:?}; the supervisor said:\n{}",
                sup.output_tail(80)
            );
        }
    }
    drop(permit);
    (sup, observer)
}

/// Waits until `unit` runs and returns its health; panics on timeout.
pub async fn await_running(observer: &zenoh::Session, unit: &str, timeout: Duration) -> Health {
    let mut watch = health_watch(observer, unit).await;
    await_health(&mut watch, timeout, |h| h.status == HealthStatus::Running).await
}

/// SIGKILLs the running `unit` and waits until the supervisor has it
/// running again under a new pid. Returns the new health.
pub async fn restart_unit(observer: &zenoh::Session, unit: &str) -> Health {
    let pid = u32::try_from(running_pid(observer, unit).await).expect("a pid fits u32");
    let mut watch = health_watch(observer, unit).await;
    unsafe {
        libc::kill(i32::try_from(pid).expect("a pid fits i32"), libc::SIGKILL);
    }
    await_health(&mut watch, Duration::from_mins(1), |h| {
        h.status == HealthStatus::Running && h.pid.is_some_and(|p| p != pid)
    })
    .await
}

/// The RFC3339 UTC instant `secs` seconds from now, to the whole second.
/// A hold may name an end at most a year ahead, so a test cannot use a
/// fixed far-future date.
pub fn rfc3339_in(secs: u64) -> String {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
        + secs;
    let (days, rem) = (at / 86_400, at % 86_400);
    // Howard Hinnant's civil_from_days, as src/pending.rs uses it.
    let z = i64::try_from(days).expect("days fit i64") + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}
