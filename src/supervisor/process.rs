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
/// `grace`, then SIGKILL the group. Returns when the direct child is reaped.
pub async fn terminate(child: &mut Child, grace: Duration) {
    let Some(pid) = child.id() else {
        return; // already reaped
    };
    signal_group(pid, libc::SIGTERM);
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        signal_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }
}

fn signal_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}
