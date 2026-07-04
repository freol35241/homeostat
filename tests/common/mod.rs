//! Harness for the supervision integration tests: spawns the real
//! `homeostat` binary on a fixture house with an isolated bus endpoint, and
//! opens an observer session to assert on bus traffic.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use homeostat::bus::{self, Health};
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
