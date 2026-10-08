//! Process spawn and termination. Every unit runs in its own process group
//! so termination can reach descendants; on Linux, `PR_SET_PDEATHSIG` makes
//! the kernel kill the child if the supervisor dies without cleaning up.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

use crate::bus::LogEntry;
use crate::supervisor::LogMap;

/// Per-unit ring buffer capacity. Memory stays bounded and the buffer is lost
/// on restart, because logs are for debugging and the durable trail lives
/// elsewhere (see docs/design.md#logs-and-the-audit-trail).
pub const LOG_CAPACITY: usize = 500;

/// Resolves the command a unit is exec'd with. For a `uv run` unit this
/// prepares the script's environment and returns that environment's
/// interpreter, invoked on the script directly. Any other command comes back
/// unchanged.
///
/// Otherwise `uv run` stays alive as the unit's parent for its whole lifetime,
/// doing nothing but `wait()`. It also holds memory while it waits: 4 MB warm
/// at best, and 25-55 MB when the environment was created or resolved from a
/// git source. In a seven-unit house these parents measured 223 MB of 443 MB
/// resident. Syncing the environment in a process that exits, then exec'ing
/// its interpreter, removes the parent. The PEP 723 metadata stays the only
/// source of the unit's dependencies, and the interpreter becomes the unit's
/// process-group leader.
///
/// Best effort: if uv cannot resolve the script (no PEP 723 block, a broken
/// dependency, no network), the original `uv run` command is returned, so the
/// failure surfaces from `uv run` itself.
pub async fn resolve(command: &str, cwd: &Path) -> String {
    let Some((script, args)) = uv_script(command) else {
        return command.to_string();
    };
    let synced = Command::new("uv")
        .args(["sync", "--script", script])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if !synced {
        return command.to_string();
    }
    let found = Command::new("uv")
        .args(["python", "find", "--script", script])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    let interpreter = match found {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => return command.to_string(),
    };
    if interpreter.is_empty() || interpreter.contains(char::is_whitespace) {
        // The command is re-tokenized on whitespace at spawn. A path that
        // would not survive that round trip is left to `uv run`.
        return command.to_string();
    }
    let mut resolved = format!("{interpreter} {script}");
    for arg in args {
        resolved.push(' ');
        resolved.push_str(arg);
    }
    resolved
}

/// The script in a `uv run [flags] <script.py> [args]` command and the
/// arguments that follow it. None for anything else, such as a binary on PATH,
/// `homeostat mcp` or a test fake; those are left as they are.
fn uv_script(command: &str) -> Option<(&str, Vec<&str>)> {
    let mut parts = command.split_whitespace();
    if parts.next()? != "uv" || parts.next()? != "run" {
        return None;
    }
    // Find the script by extension, not position. `--script` may precede it,
    // and a flag's value must not be mistaken for the script.
    let script = parts.find(|token| token.ends_with(".py"))?;
    Some((script, parts.collect()))
}

/// The supervisor environment every unit inherits regardless of its
/// manifest: what a process needs to run at all, and what `uv` and Python
/// need to find their caches, locale and CA bundles. Everything else is
/// withheld unless the manifest's `runtime.env` names it.
const BASE_ENV: [&str; 11] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TERM",
    "REQUESTS_CA_BUNDLE",
];
const BASE_ENV_PREFIXES: [&str; 5] = ["LC_", "XDG_", "UV_", "PYTHON", "SSL_CERT_"];

fn inherited(name: &str, declared: &[String]) -> bool {
    BASE_ENV.contains(&name)
        || BASE_ENV_PREFIXES.iter().any(|p| name.starts_with(p))
        || declared.iter().any(|d| d == name)
}

/// Spawns a unit command. The command string is split on whitespace and exec'd
/// directly, with no shell, so v1 manifests have no quoting. Lookup uses PATH.
/// Relative paths resolve against the house repo root, which is the cwd.
/// Stdout and stderr are piped rather than inherited, and `capture` re-emits
/// and buffers them once the child is spawned. The environment is the base set
/// plus the variables the manifest `declared`, with `env` set on top. A secret
/// handed to the supervisor for one unit must not reach the others.
pub fn spawn(
    command: &str,
    cwd: &Path,
    declared: &[String],
    env: &[(&str, &str)],
) -> io::Result<Child> {
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
        .kill_on_drop(true)
        .env_clear();
    for (key, value) in std::env::vars_os() {
        if key.to_str().is_some_and(|name| inherited(name, declared)) {
            cmd.env(key, value);
        }
    }
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
/// them. Each line is re-emitted on the supervisor's own matching stream,
/// tagged `[{unit}] `, so `docker logs` stays the raw stream and each line
/// shows its unit. Each line is also appended to the unit's ring buffer. Two
/// reader tasks run independently until their pipe closes when the unit exits.
/// This touches only the child's stdout/stderr handles and not its pid or
/// process group, so it does not interact with termination or reaping.
pub fn capture(child: &mut Child, unit: &str, log: &LogMap) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_stream(stdout, unit.to_string(), "stdout", log.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(read_stream(stderr, unit.to_string(), "stderr", log.clone()));
    }
}

/// Reads one pipe line by line until EOF, re-emitting and buffering each line.
/// Non-UTF8 bytes are decoded lossily, so malformed unit output cannot bring
/// down capture.
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
                // Append-only. The entry is created at launch and removed at
                // destroy. A final line drained after destroy must not
                // re-create it, because a phantom entry would keep the unit in
                // the served meta space and re-plan as a destroy forever.
                let Some(buffer) = buffers.get_mut(&unit) else {
                    continue;
                };
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
/// `grace`, then SIGKILL the group. It waits for the whole group and not only
/// the direct child. A unit that wraps or spawns something (a shell wrapper, a
/// relay it manages) may exit before it, and the survivor gets the rest of the
/// grace before the sweep.
pub async fn terminate(child: &mut Child, grace: Duration) {
    let Some(pid) = child.id() else {
        return; // already reaped
    };
    signal_group(pid, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    if tokio::time::timeout_at(deadline, child.wait())
        .await
        .is_err()
    {
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
    unsafe { libc::kill(-pid_t(pgid), 0) == 0 }
}

/// Sweeps a unit's process group after its leader exited on its own. A wrapper
/// or a managed child left behind would keep the unit's liveliness token alive
/// and break the next incarnation's supervision.
pub fn sweep_group(pid: u32) {
    signal_group(pid, libc::SIGKILL);
}

fn signal_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-pid_t(pid), signal);
    }
}

/// A child's id as the kernel's `pid_t`. `Child::id` is a `u32`, but the
/// kernel does not hand out a pid above `i32::MAX`.
fn pid_t(pid: u32) -> libc::pid_t {
    libc::pid_t::try_from(pid).expect("a pid fits pid_t")
}

#[cfg(test)]
mod tests {
    use super::{inherited, uv_script};

    #[test]
    fn only_the_base_set_and_declared_names_are_inherited() {
        let declared = vec!["HOMEOSTAT_NTFY_TOKEN".to_string()];
        assert!(inherited("PATH", &declared));
        assert!(inherited("LC_ALL", &declared));
        assert!(inherited("UV_CACHE_DIR", &declared));
        assert!(inherited("HOMEOSTAT_NTFY_TOKEN", &declared));
        assert!(!inherited("HOMEOSTAT_NTFY_TOKEN", &[]));
        assert!(!inherited("HOMEOSTAT_MQTT_CREDENTIALS", &declared));
        assert!(!inherited("AWS_SECRET_ACCESS_KEY", &declared));
    }

    #[test]
    fn uv_run_commands_yield_their_script_and_args() {
        assert_eq!(
            uv_script("uv run units/clock.py"),
            Some(("units/clock.py", vec![]))
        );
        assert_eq!(
            uv_script("uv run units/dashboard.py --port 8600"),
            Some(("units/dashboard.py", vec!["--port", "8600"]))
        );
        assert_eq!(
            uv_script("uv run --script units/x.py"),
            Some(("units/x.py", vec![]))
        );
        assert_eq!(
            uv_script("uv run ../../adapters/zigbee2mqtt.py"),
            Some(("../../adapters/zigbee2mqtt.py", vec![]))
        );
    }

    #[test]
    fn anything_else_is_left_alone() {
        // A binary on PATH, the core's own subcommand, a test fake: these have
        // no environment to prepare and are left alone.
        assert_eq!(uv_script("fake_adapter"), None);
        assert_eq!(uv_script("reflector"), None);
        assert_eq!(uv_script("homeostat mcp --http 0.0.0.0:8642"), None);
        assert_eq!(uv_script("uv"), None);
        assert_eq!(uv_script(""), None);
    }
}
