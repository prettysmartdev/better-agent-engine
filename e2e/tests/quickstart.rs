//! Quickstart smoke test (WI 0018 C7).
//!
//! The commands are *extracted* from `docs/guides/00-quickstart.md` (between
//! the `quickstart-commands` markers), never copied into this file, so the
//! guide and the test cannot drift. The extraction checks always run; the
//! smoke test itself is engine-gated like baectl's engine suite (see
//! `bae_e2e::engine`) and runs the extracted block verbatim, with ` --dev`
//! appended, against the local `better-agent-engine:latest` image and a
//! host-side mock provider — no real API key.
//!
//! Run it locally with `make test-quickstart` in this directory (after
//! `make image` at the repository root).

use std::net::Ipv4Addr;
use std::path::Path;

use bae_e2e::provider_mock::{self, QUICKSTART_SMOKE_REPLY};
use bae_e2e::quickstart::{
    fenced_commands_after, marked_commands, normalized_words, with_dev_flag,
    DEV_FASTEST_PATH_ANCHOR,
};

fn read_repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// The guide's copy-paste block: exactly three commands, `setup` (non-
/// interactive), `build`, `run`, in that order — the shape the smoke test's
/// provider patch between commands 1 and 2 relies on.
fn quickstart_commands() -> Vec<String> {
    let commands = marked_commands(&read_repo_file("docs/guides/00-quickstart.md"))
        .unwrap_or_else(|err| panic!("docs/guides/00-quickstart.md: {err}"));
    assert_eq!(
        commands.len(),
        3,
        "the quickstart block must hold exactly three baectl commands: {commands:?}"
    );
    for (command, verb) in commands.iter().zip(["setup", "build", "run"]) {
        assert_eq!(
            command.split_whitespace().nth(1),
            Some(verb),
            "quickstart commands must be setup, build, run: {commands:?}"
        );
    }
    assert!(
        commands[0]
            .split_whitespace()
            .any(|word| word == "--yes" || word == "-y"),
        "the quickstart's setup must be non-interactive (`--yes`): {}",
        commands[0]
    );
    commands
}

#[test]
fn quickstart_block_is_setup_build_run() {
    quickstart_commands();
}

#[test]
fn readme_repeats_the_quickstart_commands_verbatim() {
    let readme = read_repo_file("README.md");
    for command in quickstart_commands() {
        assert!(
            readme.lines().any(|line| line.trim() == command),
            "README.md's Quickstart no longer contains `{command}` verbatim"
        );
    }
}

/// The developer quickstart's fastest path is the same loop with `--dev` on
/// every verb — including `--yes`, so it is prompt-free too. Only flag order
/// is ignored.
#[test]
fn developer_fastest_path_is_the_dev_variant_of_the_quickstart() {
    let developer = fenced_commands_after(
        &read_repo_file("docs/guides/developer/00-quickstart.md"),
        DEV_FASTEST_PATH_ANCHOR,
    )
    .unwrap_or_else(|err| panic!("docs/guides/developer/00-quickstart.md: {err}"));
    let expected = with_dev_flag(&quickstart_commands());
    let normalize = |commands: &[String]| -> Vec<Vec<String>> {
        commands
            .iter()
            .map(|c| normalized_words(c).into_iter().map(str::to_owned).collect())
            .collect()
    };
    assert_eq!(
        normalize(&developer),
        normalize(&expected),
        "developer fastest path {developer:?} is not the --dev variant of {expected:?}"
    );
}

#[cfg(unix)]
#[test]
fn quickstart_commands_with_dev_print_the_agent_reply() {
    use bae_e2e::engine::{self, Workspace, SERVER_IMAGE};

    let label = "quickstart smoke test";
    let _lock = engine::lock();
    let commands = with_dev_flag(&quickstart_commands());
    let Some(baectl) = engine::require(label, &[SERVER_IMAGE]) else {
        return;
    };

    // The mock runs on its own runtime's workers while this thread blocks on
    // the baectl child processes. Bound on all interfaces: the server
    // container reaches it through `host.docker.internal` (the compose
    // `extra_hosts` entry `setup` writes).
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mock = runtime.block_on(provider_mock::start(
        Ipv4Addr::UNSPECIFIED.into(),
        provider_mock::quickstart_reply,
    ));
    let workspace = Workspace::new("quickstart", &baectl, "rust");

    // 1. setup — then swap the provider for the mock and restart the server.
    workspace.sh_ok(&commands[0]);
    workspace.point_provider_at(&format!("http://host.docker.internal:{}", mock.port()));

    // 2. build
    workspace.sh_ok(&commands[1]);

    // Between build and run, `ready` reports the two first-run fixes `run`
    // applies itself (profile + key) as ⚠ and nothing as ✗, exiting 3.
    let id = commands[2]
        .split_whitespace()
        .nth(2)
        .expect("`baectl run <id>`");
    let ready = workspace.sh(&format!("baectl ready {id} --dev"));
    let ready_stdout = String::from_utf8_lossy(&ready.stdout);
    assert_eq!(
        ready.status.code(),
        Some(3),
        "ready should exit 3 (fixable warnings only)"
    );
    assert_eq!(
        ready_stdout
            .lines()
            .filter(|line| line.contains('⚠'))
            .count(),
        2,
        "ready should print exactly two ⚠ lines:\n{ready_stdout}"
    );
    assert!(
        !ready_stdout.contains('✗'),
        "ready printed a blocking ✗:\n{ready_stdout}"
    );

    // 3. run — the agent's reply is printed.
    let run = workspace.sh_ok(&commands[2]);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains(QUICKSTART_SMOKE_REPLY),
        "`{}` did not print the agent's reply:\n{stdout}",
        commands[2]
    );
}
