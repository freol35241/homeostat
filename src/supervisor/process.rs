//! Process spawn and termination. Every unit runs in its own process group
//! so termination can reach descendants; on Linux, PR_SET_PDEATHSIG makes
//! the kernel kill the child if the supervisor dies without cleaning up.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

use crate::bus::LogEntry;
use crate::supervisor::LogMap;

/// Per-unit ring buffer capacity: bounded memory, gone on restart — logs are
/// operational exhaust, not the durable trail (see docs/design.md).
pub const LOG_CAPACITY: usize = 500;

/// Spawns a unit command. The command string is whitespace-tokenized and
/// exec'd directly — no shell, so no quoting in v1 manifests. Lookup uses
/// PATH; relative paths resolve against the house repo root (the cwd).
/// Stdout/stderr are piped, not inherited: `capture` re-emits and buffers
/// them once the child is spawned.
pub fn spawn(command: &str, cwd: &Path, env: &[(&str, &str)]) -> io::Result<Child> {
    let mut parts = command.split_whitespace();
    let argv0 = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut cmd = Command::new(argv0);
    cmd.args(parts)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Takes a freshly spawned unit's stdout/stderr pipes and starts capturing
/// them: each line is re-emitted on the supervisor's own matching stream,
/// tagged `[{unit}] ` — `docker logs` stays the raw stream, now
/// attributable — and appended to the unit's ring buffer. Two reader tasks
/// run independently until their pipe closes (the unit exits); this touches
/// only the child's stdout/stderr handles, never its pid or process group,
/// so it does not interact with termination or reaping.
pub fn capture(child: &mut Child, unit: &str, log: &LogMap) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_stream(stdout, unit.to_string(), "stdout", log.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(read_stream(stderr, unit.to_string(), "stderr", log.clone()));
    }
}

/// Reads one pipe line by line until EOF, re-emitting and buffering each
/// line. Non-UTF8 bytes are lossy-decoded — malformed unit output must
/// never bring down capture.
async fn read_stream<R>(reader: R, unit: String, stream: &'static str, log: LogMap)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf).into_owned();
                match stream {
                    "stdout" => println!("[{unit}] {line}"),
                    _ => eprintln!("[{unit}] {line}"),
                }
                let mut buffers = log.lock().expect("log map lock");
                let buffer = buffers.entry(unit.clone()).or_default();
                if buffer.len() >= LOG_CAPACITY {
                    buffer.pop_front();
                }
                buffer.push_back(LogEntry {
                    ts_us: now_us(),
                    stream: stream.to_string(),
                    line,
                });
            }
        }
    }
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_micros() as i64
}

/// Graceful termination: SIGTERM to the unit's process group, wait up to
/// `grace`, then SIGKILL the group. Waits for the whole group, not just
/// the direct child: a `uv run` wrapper exits ahead of its interpreter,
/// and the survivor gets the rest of the grace before the sweep.
pub async fn terminate(child: &mut Child, grace: Duration) {
    let Some(pid) = child.id() else {
        return; // already reaped
    };
    signal_group(pid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    if tokio::time::timeout_at(deadline, child.wait()).await.is_err() {
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
        return;
    }
    while group_alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            signal_group(pid, libc::SIGKILL);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether any member of the process group still exists.
fn group_alive(pgid: u32) -> bool {
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// Sweeps a unit's process group after its leader exited on its own. A
/// wrapper like `uv run` leaves its interpreter child behind; a survivor
/// would keep the unit's liveliness token alive and poison the next
/// incarnation's supervision.
pub fn sweep_group(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

fn signal_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
