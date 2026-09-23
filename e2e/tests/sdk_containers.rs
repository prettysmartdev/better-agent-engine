//! One packaged-container round trip per SDK (WI 0018 B11), mock-backed.
//!
//! For each bundled `reference-assistant` (Rust, TypeScript, Python): `setup
//! --yes --dev`, point the provider at a host-side mock, `build --launcher api
//! --dev` (the generated container build), `run --dev`, then trigger the
//! printed endpoint and assert the harness streamed the mock's reply. This is
//! the per-SDK container coverage baectl's own engine suite cannot give
//! without a real `ANTHROPIC_API_KEY`.
//!
//! Engine-gated (see `bae_e2e::engine`); needs `better-agent-engine:latest`
//! and `better-agent-engine:launcher-api`. Run with `make test-engine` in this
//! directory.

#![cfg(unix)]

use std::net::Ipv4Addr;
use std::process::Command;

use bae_e2e::engine::{self, Workspace, API_LAUNCHER_IMAGE, SERVER_IMAGE};
use bae_e2e::provider_mock::{self, QUICKSTART_SMOKE_REPLY};

const TRIGGER_URL: &str = "http://localhost:9090/agents/reference-assistant/trigger";

fn api_container_round_trip(sdk: &str) {
    let label = format!("{sdk} API container test");
    let _lock = engine::lock();
    let Some(baectl) = engine::require(&label, &[SERVER_IMAGE, API_LAUNCHER_IMAGE]) else {
        return;
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mock = runtime.block_on(provider_mock::start(
        Ipv4Addr::UNSPECIFIED.into(),
        provider_mock::quickstart_reply,
    ));
    let mut workspace = Workspace::new(&format!("{sdk}-api"), &baectl, sdk);
    let id = format!("reference-assistant-{sdk}-api");
    // Every SDK's API launcher publishes :9090; clear any leftover.
    for other in ["rust", "typescript", "python"] {
        engine::remove_container(&format!("reference-assistant-{other}-api"));
    }
    workspace.track_container(&id);

    workspace.sh_ok("baectl setup --yes --dev");
    workspace.point_provider_at(&format!("http://host.docker.internal:{}", mock.port()));
    workspace.sh_ok(&format!(
        "baectl build reference-assistant --sdk {sdk} --launcher api --dev"
    ));
    workspace.sh_ok(&format!("baectl run {id} --dev"));

    let curl = Command::new("curl")
        .args([
            "--fail",
            "--no-buffer",
            "--retry",
            "20",
            "--retry-connrefused",
            "--max-time",
            "180",
            "-X",
            "POST",
            TRIGGER_URL,
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"AGENT_PROMPT":"Confirm the packaged harness is running."}"#,
        ])
        .output()
        .expect("curl is available on Unix CI/dev hosts");
    let streamed = String::from_utf8_lossy(&curl.stdout);
    assert!(
        curl.status.success(),
        "trigger failed ({:?}):\n{streamed}\n{}",
        curl.status.code(),
        String::from_utf8_lossy(&curl.stderr)
    );
    assert!(
        streamed.contains(QUICKSTART_SMOKE_REPLY),
        "the packaged {sdk} harness did not stream the agent's reply:\n{streamed}"
    );
}

#[test]
fn rust_reference_assistant_builds_runs_and_answers_in_its_api_container() {
    api_container_round_trip("rust");
}

#[test]
fn typescript_reference_assistant_builds_runs_and_answers_in_its_api_container() {
    api_container_round_trip("typescript");
}

#[test]
fn python_reference_assistant_builds_runs_and_answers_in_its_api_container() {
    api_container_round_trip("python");
}
