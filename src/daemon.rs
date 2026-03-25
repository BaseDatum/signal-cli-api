use std::time::Duration;
use tokio::process::{Child, Command};

/// A managed signal-cli daemon child process.
/// Kills the entire process group on drop.
pub struct ManagedDaemon {
    child: Child,
    pid: i32,
    pub base_url: String,
}

impl Drop for ManagedDaemon {
    fn drop(&mut self) {
        kill_process_group(self.pid);
        let _ = self.child.start_kill(); // belt and braces
    }
}

/// Kill an entire process group: SIGTERM first, then SIGKILL after 2s.
/// Public so integration tests can call it directly.
pub fn kill_process_group(pid: i32) {
    // Send SIGTERM to the process group (negative PID = group)
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    // Give processes time to exit gracefully
    std::thread::sleep(Duration::from_secs(2));
    // Escalate to SIGKILL for anything still alive
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// Find signal-cli on $PATH.
fn find_signal_cli() -> anyhow::Result<String> {
    let output = std::process::Command::new("which")
        .arg("signal-cli")
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        anyhow::bail!(
            "signal-cli not found on $PATH. Install it or use --signal-cli <url> to connect to an existing daemon"
        )
    }
}

/// Spawn signal-cli daemon in HTTP mode on a random available port and
/// wait until it's ready. The child is placed in its own process group
/// via setsid() so that dropping ManagedDaemon kills the entire tree.
pub async fn spawn() -> anyhow::Result<ManagedDaemon> {
    let bin = find_signal_cli()?;
    tracing::info!("Found signal-cli at {bin}");

    // Grab a random available port by binding then releasing.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };
    let addr = format!("127.0.0.1:{port}");
    let base_url = format!("http://{addr}");

    tracing::info!("Spawning signal-cli daemon (HTTP mode) on {addr}");
    // SAFETY: pre_exec runs in the forked child before exec. setsid() is
    // async-signal-safe and creates a new session/process group, which lets
    // us kill the entire group (including Java grandchildren) on shutdown.
    let mut child = unsafe {
        Command::new(&bin)
            .args(["daemon", "--http", &addr, "--no-receive-stdout", "--receive-mode", "on-start"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .pre_exec(|| {
                let ret = libc::setsid();
                if ret == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()?
    };

    let pid = child.id().expect("child should have a PID") as i32;

    // Log child stdout/stderr in background tasks so they don't fill the pipe buffer.
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "signal_cli", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "signal_cli", "{line}");
            }
        });
    }

    // Poll the HTTP health endpoint until ready (max ~30s).
    let client = reqwest::Client::new();
    let health_url = format!("{base_url}/api/v1/check");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("signal-cli daemon failed to start within 30 seconds");
        }
        // Check if the child exited early (crash/error).
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("signal-cli exited with {status}");
        }
        match client.get(&health_url).timeout(Duration::from_secs(2)).send().await {
            Ok(resp) if resp.status().is_success() => break,
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    tracing::info!("signal-cli daemon ready at {base_url}");

    Ok(ManagedDaemon { child, pid, base_url })
}
