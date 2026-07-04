//! Process spawn and termination. Every unit runs in its own process group
//! so termination can reach descendants; on Linux, PR_SET_PDEATHSIG makes
//! the kernel kill the child if the supervisor dies without cleaning up.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// Spawns a unit command. The command string is whitespace-tokenized and
/// exec'd directly — no shell, so no quoting in v1 manifests. Lookup uses
/// PATH; relative paths resolve against the house repo root (the cwd).
pub fn spawn(command: &str, cwd: &Path, env: &[(&str, &str)]) -> io::Result<Child> {
    let mut parts = command.split_whitespace();
    let argv0 = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut cmd = Command::new(argv0);
    cmd.args(parts)
        .current_dir(cwd)
        .stdin(Stdio::null())
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
