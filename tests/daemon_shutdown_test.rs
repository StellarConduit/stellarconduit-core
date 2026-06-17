/// Integration test: spawn the stellarconduitd binary, send SIGTERM, assert it exits within 5s.
#[cfg(unix)]
#[tokio::test]
async fn test_daemon_exits_within_5_seconds() {
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::time::timeout;

    // Build the binary first.
    let build = std::process::Command::new("cargo")
        .args(["build", "--bin", "stellarconduitd", "--quiet"])
        .status()
        .expect("cargo build failed");
    assert!(build.success(), "binary failed to build");

    let mut child = Command::new("cargo")
        .args(["run", "--bin", "stellarconduitd", "--quiet", "--", "--db-path", ":memory:"])
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn stellarconduitd");

    // Give the daemon a moment to initialize.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Send SIGTERM via the `kill` shell command (no libc dependency needed).
    let pid = child.id().expect("failed to get child pid");
    std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill command failed");

    let result = timeout(Duration::from_secs(5), child.wait()).await;
    assert!(
        result.is_ok(),
        "daemon did not exit within 5 seconds after SIGTERM"
    );
}
