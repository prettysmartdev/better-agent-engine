//! Offline process-level coverage for the multi-agent scheduler.
//!
//! The harnesses are `/bin/sh` commands and communicate only through files in
//! the OS temp directory. No network listener, provider key, or external
//! service is involved.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "baesched-integration-{label}-{}.txt",
        std::process::id()
    ))
}

fn read_count(path: &PathBuf) -> usize {
    fs::read_to_string(path)
        .map(|body| body.lines().count())
        .unwrap_or(0)
}

#[test]
fn independent_agents_fire_slow_overlap_skips_and_hung_isolation_holds() {
    let slow = temp_path("slow");
    let fast = temp_path("fast");
    let hung = temp_path("hung");
    let crasher = temp_path("crasher");
    let config = temp_path("config");
    let config_body = format!(
        r#"
[[agents]]
name = "slow"
command = "/bin/sh"
args = ["-c", "echo slow >> {slow}; sleep 2"]
schedule = "*/1 * * * * *"

[[agents]]
name = "fast"
command = "/bin/sh"
args = ["-c", "echo fast >> {fast}; echo fast-out"]
schedule = "*/1 * * * * *"

[[agents]]
name = "hung"
command = "/bin/sh"
args = ["-c", "echo hung >> {hung}; sleep 60"]
schedule = "*/1 * * * * *"

[[agents]]
name = "crasher"
command = "/bin/sh"
args = ["-c", "echo crash >> {crasher}; exit 3"]
schedule = "*/1 * * * * *"
"#,
        slow = slow.display(),
        fast = fast.display(),
        hung = hung.display(),
        crasher = crasher.display(),
    );
    fs::write(&config, config_body).expect("write scheduler fixture");

    let stdout_file = temp_path("stdout");
    let stderr_file = temp_path("stderr");
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_baesched"))
        .env("BAE_SCHEDULES_CONFIG", &config)
        .env("BAE_SCHEDULES_SHUTDOWN_TIMEOUT", "1")
        .env("BAE_LOG", "warn")
        .stdout(fs::File::create(&stdout_file).expect("stdout capture"))
        .stderr(fs::File::create(&stderr_file).expect("stderr capture"))
        .spawn()
        .expect("start scheduler");

    // At least two fires are expected for the fast agent. The slow agent's
    // second fire overlaps its first and must be skipped, while the hung
    // invocation and the every-fire-crashing agent must not prevent fast from
    // continuing to fire.
    thread::sleep(Duration::from_millis(3_500));
    let pid = launcher.id().to_string();
    let signal_status = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {pid}")])
        .status()
        .expect("send SIGTERM");
    assert!(signal_status.success());
    let status = launcher.wait().expect("wait for scheduler");

    let fast_count = read_count(&fast);
    let slow_count = read_count(&slow);
    let hung_count = read_count(&hung);
    let crash_count = read_count(&crasher);
    assert!(status.success(), "scheduler exited with {status}");
    assert!(fast_count >= 2, "fast agent fired only {fast_count} times");
    assert!(slow_count >= 1, "slow agent never fired");
    assert!(slow_count < fast_count, "slow overlap was not skipped");
    assert!(hung_count >= 1, "hung agent never fired");
    // A nonzero-exit child is a per-invocation event: the schedule keeps
    // firing it, and the launcher stays up.
    assert!(
        crash_count >= 2,
        "crashing agent stopped being scheduled after {crash_count} fires"
    );

    // The overlap skip is logged with the exact documented warning.
    let warnings = fs::read_to_string(&stderr_file).unwrap_or_default();
    assert!(
        warnings.contains("agent \"slow\" skipped: previous invocation still running"),
        "documented overlap-skip warning missing: {warnings:?}"
    );
    // Child stdout is forwarded to the launcher's own stdout, name-prefixed.
    let forwarded = fs::read_to_string(&stdout_file).unwrap_or_default();
    assert!(
        forwarded.contains("[fast] fast-out"),
        "child stdout not forwarded to launcher stdout: {forwarded:?}"
    );

    for path in [slow, fast, hung, crasher, config, stdout_file, stderr_file] {
        fs::remove_file(path).ok();
    }
}
