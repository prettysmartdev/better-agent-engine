//! `baesched` — cron-triggered launcher for one or more independent harnesses.
//!
//! The configuration is deliberately only an `[[agents]]` array: every entry
//! gets an independent cron job and its own same-agent overlap guard. There is
//! intentionally no HTTP listener; process/container status is its liveness
//! surface. Child process mechanics are shared with `launcher-core`.

use std::collections::HashMap;
use std::env;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use launcher_core::{
    init_logging, resolve_env_refs, spawn_and_stream, validate_unique_names, LauncherError,
    LogLine, OutputStream, SpawnSpec,
};
use serde::Deserialize;
use tokio::signal;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

const DEFAULT_CONFIG_PATH: &str = "/etc/bae/bae-schedules.toml";
const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// The sole TOML top-level shape. `deny_unknown_fields` intentionally rejects
/// a legacy/singular `[agent]` block rather than silently starting zero jobs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleConfig {
    #[serde(default)]
    agents: Vec<ScheduleAgentConfig>,
}

/// A richer config type local to the scheduler; it becomes a core `SpawnSpec`
/// only immediately before an invocation starts, after env-reference resolution.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleAgentConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    working_dir: Option<String>,
    schedule: String,
}

#[derive(Debug)]
enum StartupError {
    ConfigRead {
        path: String,
        source: std::io::Error,
    },
    ConfigParse {
        path: String,
        source: toml::de::Error,
    },
    Core(LauncherError),
    Cron {
        agent: String,
        message: String,
    },
    Scheduler(String),
    InvalidShutdownTimeout {
        value: String,
    },
}

impl StartupError {
    fn exit_code(&self) -> i32 {
        match self {
            StartupError::Core(error) => error.exit_code(),
            // Every startup failure here is an operator configuration/usage
            // error; child/runtime failures happen per invocation and are logged.
            StartupError::ConfigRead { .. }
            | StartupError::ConfigParse { .. }
            | StartupError::Cron { .. }
            | StartupError::InvalidShutdownTimeout { .. } => 2,
            StartupError::Scheduler(_) => 1,
        }
    }
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigRead { path, source } => {
                write!(f, "failed to read schedule config {path:?}: {source}")
            }
            Self::ConfigParse { path, source } => {
                write!(f, "invalid schedule config {path:?}: {source}")
            }
            Self::Core(error) => error.fmt(f),
            Self::Cron { agent, message } => {
                write!(f, "invalid schedule for agent {agent:?}: {message}")
            }
            Self::Scheduler(message) => write!(f, "failed to initialize scheduler: {message}"),
            Self::InvalidShutdownTimeout { value } => write!(
                f,
                "BAE_SCHEDULES_SHUTDOWN_TIMEOUT must be a whole number of seconds, got {value:?}"
            ),
        }
    }
}

impl std::error::Error for StartupError {}

/// Holds an overlap flag until an invocation task returns, including when it
/// exits through a normal child failure path.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging("info");

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %error, "baesched failed to start");
            ExitCode::from(error.exit_code() as u8)
        }
    }
}

async fn run() -> Result<(), StartupError> {
    let config_path =
        env::var("BAE_SCHEDULES_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.into());
    let agents = load_config(&config_path)?;
    let shutdown_timeout = shutdown_timeout()?;

    let mut scheduler = JobScheduler::new()
        .await
        .map_err(|error| StartupError::Scheduler(error.to_string()))?;
    let invocation_tasks = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    let accepting_fires = Arc::new(AtomicBool::new(true));

    // One registration pass is deliberately O(N): V1 permits any practical
    // number of agents, but has no hard cap or cross-agent coordination.
    for agent in agents {
        let job = schedule_job(
            agent,
            Arc::clone(&invocation_tasks),
            Arc::clone(&accepting_fires),
        )?;
        scheduler
            .add(job)
            .await
            .map_err(|error| StartupError::Scheduler(error.to_string()))?;
    }

    scheduler
        .start()
        .await
        .map_err(|error| StartupError::Scheduler(error.to_string()))?;
    info!("schedule launcher started; waiting for SIGTERM or SIGINT");

    wait_for_shutdown_signal().await;
    info!("shutdown signal received; stopping new scheduled fires");
    accepting_fires.store(false, Ordering::Release);
    // A received termination signal is a clean process exit. If the scheduler
    // reports an internal shutdown error, log it but still drain/kill children
    // and return 0 rather than turning container termination into a failure.
    if let Err(error) = scheduler.shutdown().await {
        warn!(error = %error, "scheduler reported an error while stopping fires");
    }

    finish_invocations(invocation_tasks, shutdown_timeout).await;
    info!("schedule launcher stopped");
    Ok(())
}

fn load_config(path: &str) -> Result<Vec<ScheduleAgentConfig>, StartupError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warn!(
                path,
                "schedule config file is absent; starting with zero agents"
            );
            return Ok(Vec::new());
        }
        Err(source) => {
            return Err(StartupError::ConfigRead {
                path: path.to_owned(),
                source,
            })
        }
    };

    let config: ScheduleConfig =
        toml::from_str(&contents).map_err(|source| StartupError::ConfigParse {
            path: path.to_owned(),
            source,
        })?;
    validate_unique_names(config.agents.iter().map(|agent| agent.name.as_str()))
        .map_err(StartupError::Core)?;
    for agent in &config.agents {
        if agent.schedule.split_whitespace().count() != 6 {
            return Err(StartupError::Cron {
                agent: agent.name.clone(),
                message: "expected a six-field cron expression: sec min hour day month day-of-week"
                    .to_owned(),
            });
        }
    }
    Ok(config.agents)
}

fn shutdown_timeout() -> Result<Duration, StartupError> {
    let value = match env::var("BAE_SCHEDULES_SHUTDOWN_TIMEOUT") {
        Ok(value) => value,
        Err(_) => return Ok(Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS)),
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| StartupError::InvalidShutdownTimeout { value })?;
    Ok(Duration::from_secs(seconds))
}

fn schedule_job(
    agent: ScheduleAgentConfig,
    invocation_tasks: Arc<tokio::sync::Mutex<JoinSet<()>>>,
    accepting_fires: Arc<AtomicBool>,
) -> Result<Job, StartupError> {
    let agent_name = agent.name.clone();
    let agent_name_for_error = agent_name.clone();
    let running = Arc::new(AtomicBool::new(false));
    let schedule = agent.schedule.clone();
    Job::new_async(schedule.as_str(), move |_job_id, _scheduler| {
        let agent = agent.clone();
        let agent_name = agent_name.clone();
        let running = Arc::clone(&running);
        let invocation_tasks = Arc::clone(&invocation_tasks);
        let accepting_fires = Arc::clone(&accepting_fires);
        Box::pin(async move {
            if !accepting_fires.load(Ordering::Acquire) {
                return;
            }
            if running.swap(true, Ordering::AcqRel) {
                warn!(agent = %agent_name, "agent \"{}\" skipped: previous invocation still running", agent_name);
                return;
            }

            // The scheduler callback stays short. Each fire owns a separate
            // task so a slow/hung child cannot block this or another agent's job.
            let mut tasks = invocation_tasks.lock().await;
            // Keep tracking bounded by live invocations, rather than retaining
            // every completed fire for the life of the scheduler.
            while tasks.try_join_next().is_some() {}
            // This second check is while holding the same lock shutdown later
            // holds to drain tasks. It closes the signal race between the
            // initial check and task registration.
            if !accepting_fires.load(Ordering::Acquire) {
                running.store(false, Ordering::Release);
                return;
            }
            tasks.spawn(async move {
                let _running = RunningGuard(running);
                run_invocation(agent).await;
            });
        })
    })
    .map_err(|error| StartupError::Cron {
        agent: agent_name_for_error,
        message: error.to_string(),
    })
}

async fn run_invocation(agent: ScheduleAgentConfig) {
    let env = match resolve_env_refs(&agent.env, &|key| env::var(key).ok()) {
        Ok(env) => env,
        Err(error) => {
            error!(agent = %agent.name, error = %error, "agent invocation failed before spawn");
            return;
        }
    };

    let mut spec = SpawnSpec::new(agent.name.clone(), agent.command, agent.args);
    spec.env = env;
    spec.working_dir = agent.working_dir;
    let stream = spawn_and_stream(&spec);
    tokio::pin!(stream);

    while let Some(line) = stream.next().await {
        match line {
            LogLine::Output {
                stream: OutputStream::Stdout,
                line,
            } => println!("{line}"),
            LogLine::Output {
                stream: OutputStream::Stderr,
                line,
            } => eprintln!("{line}"),
            LogLine::Exited { code: Some(0) } => {
                info!(agent = %spec.name, "agent invocation completed");
            }
            LogLine::Exited { code } => {
                warn!(agent = %spec.name, ?code, "agent invocation exited unsuccessfully");
            }
            LogLine::SpawnFailed { message } => {
                error!(agent = %spec.name, %message, "agent invocation could not start");
            }
        }
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal as unix_signal, SignalKind};

        let mut terminate = unix_signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

async fn finish_invocations(
    invocation_tasks: Arc<tokio::sync::Mutex<JoinSet<()>>>,
    grace_period: Duration,
) {
    let mut tasks = invocation_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }

    info!(
        seconds = grace_period.as_secs(),
        "waiting for in-flight agent invocations"
    );
    if timeout(grace_period, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        warn!("shutdown grace period elapsed; force-killing in-flight agent invocations");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        // Yield once so the aborted task drops its core output stream. Its
        // `kill_on_drop(true)` child handle then force-kills/reaps the child.
        sleep(Duration::ZERO).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture(contents: &str) -> PathBuf {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("baesched-test-{}-{id}.toml", std::process::id()));
        fs::write(&path, contents).expect("write fixture");
        path
    }

    fn valid_agents() -> &'static str {
        r#"
[[agents]]
name = "alpha"
command = "echo"
args = ["alpha"]
schedule = "*/5 * * * * *"

[[agents]]
name = "beta"
command = "echo"
args = ["beta"]
schedule = "*/7 * * * * *"
"#
    }

    #[test]
    fn valid_config_parses_as_two_agents() {
        let path = fixture(valid_agents());
        let agents = load_config(path.to_str().unwrap()).expect("valid config");
        fs::remove_file(path).ok();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "alpha");
        assert_eq!(agents[1].name, "beta");
    }

    #[test]
    fn malformed_toml_is_fatal_usage_error() {
        let path = fixture(
            r#"
[[agents]]
name = "alpha"
command = [not valid
schedule = "*/5 * * * * *"

[[agents]]
name = "beta"
command = "echo"
schedule = "*/7 * * * * *"
"#,
        );
        let error = load_config(path.to_str().unwrap()).expect_err("malformed TOML");
        fs::remove_file(path).ok();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("invalid schedule config"));
    }

    #[test]
    fn duplicate_name_names_the_offending_agent_and_is_exit_two() {
        let path = fixture(&valid_agents().replace("name = \"beta\"", "name = \"alpha\""));
        let error = load_config(path.to_str().unwrap()).expect_err("duplicate name");
        fs::remove_file(path).ok();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("duplicate agent name \"alpha\""));
    }

    #[test]
    fn invalid_cron_is_fatal_and_names_the_agent() {
        let path = fixture(&valid_agents().replace("*/7 * * * * *", "never-a-cron * * * * *"));
        let agents = load_config(path.to_str().unwrap()).expect("six fields are parsed first");
        fs::remove_file(path).ok();

        let tasks = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
        let accepting = Arc::new(AtomicBool::new(true));
        let error = match schedule_job(agents.into_iter().nth(1).unwrap(), tasks, accepting) {
            Ok(_) => panic!("invalid cron must fail during job registration"),
            Err(error) => error,
        };
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("beta"));
    }

    #[test]
    fn one_agent_config_is_the_same_supported_shape() {
        let path = fixture(
            r#"
[[agents]]
name = "only"
command = "echo"
schedule = "*/5 * * * * *"
"#,
        );
        let agents = load_config(path.to_str().unwrap()).expect("single agent remains valid");
        fs::remove_file(path).ok();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "only");
    }

    #[test]
    fn tens_of_agents_parse_in_one_pass_with_no_cap() {
        let mut contents = String::new();
        for i in 0..30 {
            contents.push_str(&format!(
                "[[agents]]\nname = \"agent-{i:02}\"\ncommand = \"echo\"\nschedule = \"*/5 * * * * *\"\n\n"
            ));
        }
        let path = fixture(&contents);
        let agents = load_config(path.to_str().unwrap()).expect("30 agents parse");
        fs::remove_file(path).ok();
        assert_eq!(agents.len(), 30);
        assert_eq!(agents[0].name, "agent-00");
        assert_eq!(agents[29].name, "agent-29");
    }

    #[test]
    fn missing_config_starts_with_zero_agents() {
        let path = env::temp_dir().join(format!(
            "baesched-missing-{}-{}.toml",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let agents = load_config(path.to_str().unwrap()).expect("missing config is nonfatal");
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn env_reference_resolves_immediately_before_schedule_spawn() {
        let output = env::temp_dir().join(format!(
            "baesched-env-{}-{}.txt",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let variable = format!("BAESCHED_TEST_SECRET_{}", std::process::id());
        env::set_var(&variable, "resolved-value");
        let agent = ScheduleAgentConfig {
            name: "env-agent".to_string(),
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("printf '%s' \"$TOKEN\" > {}", output.display()),
            ],
            env: HashMap::from([("TOKEN".to_string(), format!("${{{variable}}}"))]),
            working_dir: None,
            schedule: "*/5 * * * * *".to_string(),
        };

        run_invocation(agent).await;
        assert_eq!(fs::read_to_string(&output).unwrap(), "resolved-value");
        env::remove_var(variable);
        fs::remove_file(output).ok();
    }

    #[tokio::test]
    async fn unset_env_reference_fails_one_invocation_without_spawning() {
        let output = env::temp_dir().join(format!(
            "baesched-unset-env-{}-{}.txt",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let variable = format!("BAESCHED_TEST_UNSET_{}", std::process::id());
        env::remove_var(&variable);
        let agent = ScheduleAgentConfig {
            name: "missing-env-agent".to_string(),
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("echo spawned > {}", output.display()),
            ],
            env: HashMap::from([("TOKEN".to_string(), format!("${{{variable}}}"))]),
            working_dir: None,
            schedule: "*/5 * * * * *".to_string(),
        };

        run_invocation(agent).await;
        assert!(!output.exists());
        fs::remove_file(output).ok();
    }
}
