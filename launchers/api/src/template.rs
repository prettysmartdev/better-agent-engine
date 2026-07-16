//! Turning a validated request body into a [`launcher_core::SpawnSpec`].
//!
//! Two independent copy operations, applied only after JSON Schema validation
//! has passed:
//!
//! - **`env_template`**: each entry copies one body field's value into a
//!   child-process env var.
//! - **`arg_template`**: each entry appends `flag` then one body field's value
//!   to the child's CLI args (after the agent's base `args`).
//!
//! A body field referenced by a template entry but absent from the (schema-valid)
//! body is simply skipped — `request_schema` decides what is required; the
//! template layer never invents a value.
//!
//! # `${VAR}` boundary
//!
//! Everything **operator-authored** may carry `${VAR}` references, resolved
//! against the launcher's own environment immediately before each spawn (work
//! item 0014 section A): the agent's static [`AgentConfig::env`] values, an
//! `env_template` entry's `env` string, and an `arg_template` entry's `flag`
//! string. An unset referenced variable is a hard [`LauncherError`] for that
//! invocation (mapped to a 500 by the caller), never an empty-string or literal
//! substitution.
//!
//! Values copied from the **request body** are passed through verbatim and are
//! *never* `${VAR}`-resolved: a request is untrusted input, and resolving
//! `${SECRET}` out of it would let any caller exfiltrate the launcher's
//! environment through the streamed response.

use launcher_core::{LauncherError, SpawnSpec};
use serde_json::Value;

use crate::config::AgentConfig;

/// Build the [`SpawnSpec`] for one trigger of `agent` with request `body`.
///
/// `environ` is the injectable environment lookup used to resolve `${VAR}` in
/// the agent's static `env` and in the applied template entries' own `env`/
/// `flag` strings (production passes `&|k| std::env::var(k).ok()`). Returns
/// [`LauncherError::MissingEnv`] if a referenced variable is unset — that
/// invocation fails; nothing is substituted literally or as an empty string.
pub fn build_spec(
    agent: &AgentConfig,
    body: &Value,
    environ: &dyn Fn(&str) -> Option<String>,
) -> Result<SpawnSpec, LauncherError> {
    // Static env first (a missing ${VAR} is a hard error here, surfaced as a
    // 500 upstream).
    let mut env = launcher_core::resolve_env_refs(&agent.env, environ)?;

    // Template entries: the operator-authored strings (`env`, `flag`) are
    // ${VAR}-resolved per invocation; the body-derived values are copied
    // verbatim (never ${VAR}-resolved — untrusted input).
    if let Some(obj) = body.as_object() {
        for entry in &agent.env_template {
            if let Some(value) = obj.get(&entry.field) {
                env.insert(
                    resolve_config_str(&entry.env, environ)?,
                    value_to_string(value),
                );
            }
        }
    }

    let mut args = agent.args.clone();
    if let Some(obj) = body.as_object() {
        for entry in &agent.arg_template {
            if let Some(value) = obj.get(&entry.field) {
                args.push(resolve_config_str(&entry.flag, environ)?);
                args.push(value_to_string(value));
            }
        }
    }

    Ok(SpawnSpec {
        name: agent.name.clone(),
        command: agent.command.clone(),
        args,
        env,
        working_dir: agent.working_dir.clone(),
    })
}

/// Resolve `${VAR}` references in one operator-authored config string (an
/// `env_template` entry's `env`, or an `arg_template` entry's `flag`) against
/// the launcher's environment. `launcher_core` deliberately only exposes the
/// map-shaped resolver, so a one-entry map wraps the single string.
fn resolve_config_str(
    value: &str,
    environ: &dyn Fn(&str) -> Option<String>,
) -> Result<String, LauncherError> {
    let map = std::collections::HashMap::from([(String::new(), value.to_owned())]);
    let mut resolved = launcher_core::resolve_env_refs(&map, environ)?;
    Ok(resolved.remove("").expect("key preserved by resolver"))
}

/// Render a JSON value as a plain env/arg string.
///
/// A JSON string becomes its raw contents (no surrounding quotes); `null`
/// becomes the empty string; every other value (number, bool, array, object)
/// becomes its compact JSON form — the least-surprising shape for a harness that
/// reads `AGENT_PROMPT` or a `--flag` value.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArgTemplate, EnvTemplate};
    use std::collections::HashMap;

    fn agent() -> AgentConfig {
        AgentConfig {
            name: "templater".to_string(),
            command: "harness".to_string(),
            args: vec!["--base".to_string()],
            working_dir: None,
            env: HashMap::from([("TOKEN".to_string(), "${SECRET}".to_string())]),
            request_schema: None,
            env_template: vec![EnvTemplate {
                field: "prompt".to_string(),
                env: "AGENT_PROMPT".to_string(),
            }],
            arg_template: vec![ArgTemplate {
                field: "priority".to_string(),
                flag: "--priority".to_string(),
            }],
            display_name: None,
            description: None,
            icon: None,
            chat_input_field: "prompt".to_string(),
            prompts: Vec::new(),
        }
    }

    #[test]
    fn valid_body_and_static_env_are_templated() {
        let spec = build_spec(
            &agent(),
            &serde_json::json!({"prompt": "hello", "priority": 3}),
            &|name| (name == "SECRET").then(|| "secret-value".to_string()),
        )
        .expect("template succeeds");
        assert_eq!(spec.env["TOKEN"], "secret-value");
        assert_eq!(spec.env["AGENT_PROMPT"], "hello");
        assert_eq!(spec.args, ["--base", "--priority", "3"]);
    }

    #[test]
    fn unset_static_env_is_a_hard_failure() {
        let error = build_spec(&agent(), &serde_json::json!({"prompt": "hello"}), &|_| None)
            .expect_err("missing secret");
        assert_eq!(
            error,
            LauncherError::MissingEnv {
                var: "SECRET".to_string()
            }
        );
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn request_values_are_literal_and_missing_template_fields_are_skipped() {
        let spec = build_spec(
            &agent(),
            &serde_json::json!({"prompt": "${NOT_RESOLVED}"}),
            &|name| (name == "SECRET").then(|| "secret-value".to_string()),
        )
        .expect("template succeeds");
        assert_eq!(spec.env["AGENT_PROMPT"], "${NOT_RESOLVED}");
        assert_eq!(spec.args, ["--base"]);
    }

    #[test]
    fn template_entry_strings_resolve_env_refs_per_invocation() {
        let mut agent = agent();
        agent.env.clear();
        agent.env_template[0].env = "${TARGET_ENV_NAME}".to_string();
        agent.arg_template[0].flag = "${PRIORITY_FLAG}".to_string();
        let spec = build_spec(
            &agent,
            &serde_json::json!({"prompt": "hello", "priority": 3}),
            &|name| match name {
                "TARGET_ENV_NAME" => Some("AGENT_PROMPT".to_string()),
                "PRIORITY_FLAG" => Some("--priority".to_string()),
                _ => None,
            },
        )
        .expect("template strings resolve");
        assert_eq!(spec.env["AGENT_PROMPT"], "hello");
        assert!(!spec.env.contains_key("${TARGET_ENV_NAME}"));
        assert_eq!(spec.args, ["--base", "--priority", "3"]);
    }

    #[test]
    fn unset_env_ref_in_template_entry_is_a_hard_failure_not_a_literal() {
        let mut agent = agent();
        agent.env.clear();
        agent.env_template[0].env = "${DEFINITELY_UNSET_LAUNCHER_TARGET}".to_string();
        let error = build_spec(&agent, &serde_json::json!({"prompt": "hello"}), &|_| None)
            .expect_err("unset ${VAR} in an applied env_template entry must fail");
        assert_eq!(
            error,
            LauncherError::MissingEnv {
                var: "DEFINITELY_UNSET_LAUNCHER_TARGET".to_string()
            }
        );
    }
}
