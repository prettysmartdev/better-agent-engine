//! Reverse guard for `baectl build`'s generated launcher configs.
//!
//! `baectl build --launcher api|webapp` hand-builds `bae-api.toml` /
//! `bae-app.toml`. baectl pins that output byte-for-byte in
//! `baectl/tests/fixtures/generated/` (its own test regenerates and compares),
//! and this test loads those exact files through `baeapi`'s real config parser —
//! `deny_unknown_fields` and the JSON-Schema compile included. The dependency
//! points from the launcher to baectl's checked-in fixtures, so baectl carries
//! no path dependency on this crate (which would break its Docker image build).

use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../baectl/tests/fixtures/generated")
        .join(name)
}

#[test]
fn baectl_generated_api_and_webapp_configs_load_through_the_real_parser() {
    for name in ["bae-api.toml", "bae-app.toml"] {
        let path = fixture(name);
        let loaded = launcher_api::config::load(path.to_str().unwrap()).unwrap_or_else(|e| {
            panic!(
                "baectl's generated {name} ({}) no longer loads in baeapi: {e:?}",
                path.display()
            )
        });

        assert_eq!(loaded.addr.as_deref(), Some("0.0.0.0:9090"), "{name}");
        assert_eq!(loaded.agents.len(), 1, "{name}");
        let agent = &loaded.agents[0];
        assert_eq!(agent.config.name, "reference-assistant");
        assert_eq!(agent.config.command, "/usr/local/bin/reference-assistant");
        assert_eq!(agent.config.env_template.len(), 1);
        assert_eq!(agent.config.env_template[0].field, "AGENT_PROMPT");
        assert_eq!(agent.config.env_template[0].env, "AGENT_PROMPT");
        // The compiled validator proves the generated request_schema is a valid
        // JSON Schema, not merely parseable TOML.
        assert!(agent.validator.is_some(), "{name}");
    }
}

/// `config::load` treats a *missing* file as "zero agents", so a moved or
/// renamed fixture must fail here rather than silently pass the guard above.
#[test]
fn baectl_generated_fixtures_exist_at_the_guarded_path() {
    for name in ["bae-api.toml", "bae-app.toml"] {
        let path = fixture(name);
        assert!(
            path.is_file(),
            "{} is missing — baectl's generated-config fixtures moved?",
            path.display()
        );
    }
}

/// Negative control: the guard only proves something if the real parser
/// rejects drift. An unknown key added to baectl's generated output (at the top
/// level or inside an agent) must fail to load.
#[test]
fn drift_in_baectl_generated_configs_is_rejected_by_the_real_parser() {
    let original = std::fs::read_to_string(fixture("bae-api.toml")).unwrap();
    let dir = std::env::temp_dir().join(format!("baeapi-baectl-drift-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (label, drifted) in [
        ("top-level", format!("unexpected_key = true\n{original}")),
        (
            "agent",
            original.replacen("command = ", "unexpected_agent_key = \"x\"\ncommand = ", 1),
        ),
    ] {
        assert_ne!(drifted, original, "{label}: the drift must apply");
        let path = dir.join(format!("{label}.toml"));
        std::fs::write(&path, drifted).unwrap();
        assert!(
            launcher_api::config::load(path.to_str().unwrap()).is_err(),
            "{label}: an unknown key must be rejected"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
