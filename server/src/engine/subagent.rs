//! Remote CLI subagents executed inside an already-started session sandbox.
//!
//! Mirrors the small, hand-rolled subprocess surface in [`super::sandbox`].
//!
//! # Why hand-rolled
//!
//! A remote subagent is one `docker`/`container exec -i … sh -c` invocation,
//! not a new container API or a host shell capability. Reusing the sandbox
//! module's [`super::sandbox::CommandRunner`] keeps the dependency graph small
//! and lets tests script the subprocess entirely offline.
//!
//! # Lifecycle
//!
//! `run_turn` validates a declared subagent against the session's already
//! started sandbox, records `start`, retains a [`SubagentTask`] in AppState,
//! and spawns the command in the background. The status tool owns terminal
//! acknowledgement and eviction; close/cancel abort the task (the production
//! runner uses `kill_on_drop`). There is deliberately no remote bare-host
//! execution shape.
//!
//! # Post-turn events
//!
//! Completion/failure is emitted by the detached task, so this is the one
//! dispatch path that can publish lifecycle events after its triggering turn
//! has ended.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::task::JoinHandle;

/// Server-generated remote subagent ids use the same random-hex convention as
/// session/event ids.
pub use crate::store::sessions::SUBAGENT_ID_PREFIX;
pub const SUBAGENT_OUTPUT_CAP_BYTES: usize = 65_536;

pub const REMOTE_STATUS_TOOL_NAME: &str = "remote_subagent_status";
pub const STATUS_TOOL_DESCRIPTION: &str = "Check the status of subagents launched with launch_subagent. Pass a subagent_id to query one subagent, or omit it to list all tracked subagents. A subagent that has finished is reported with its captured output exactly once.";

/// Process-wide monotonic order. Session status only compares tasks within one
/// session, so a global sequence is sufficient and cannot tie like a wall clock.
static NEXT_LAUNCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A declaration kept server-side; only its public tool fields are advertised
/// to the provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubagentToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    pub image: String,
    pub subagents: Vec<SubagentDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubagentDef {
    pub harness: String,
    pub command_template: String,
    #[serde(default = "default_prompt_via")]
    pub prompt_via: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn default_prompt_via() -> String {
    "stdin".to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
}

impl SubagentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// The in-memory authoritative record for a remotely launched subagent.
pub struct SubagentTask {
    pub harness: String,
    pub model: String,
    pub status: SubagentStatus,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub detail: Option<String>,
    pub launch_sequence: u64,
    pub task: Option<JoinHandle<()>>,
}

impl SubagentTask {
    pub fn running(harness: String, model: String) -> Self {
        Self {
            harness,
            model,
            status: SubagentStatus::Running,
            stdout: None,
            stderr: None,
            truncated: false,
            exit_code: None,
            reason: None,
            detail: None,
            launch_sequence: NEXT_LAUNCH_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            task: None,
        }
    }

    pub fn status_json(&self, id: &str) -> Value {
        json!({
            "subagent_id": id, "harness": self.harness, "model": self.model,
            "status": self.status.as_str(), "exit_code": self.exit_code,
            "stdout": self.stdout, "stderr": self.stderr,
            "truncated": self.truncated, "reason": self.reason,
            "detail": self.detail,
        })
    }
}

/// The public, synthesized status-tool definition.
pub fn status_tool_definition() -> Value {
    json!({
        "name": REMOTE_STATUS_TOOL_NAME,
        "description": STATUS_TOOL_DESCRIPTION,
        "input_schema": {
            "type": "object",
            "properties": { "subagent_id": { "type": "string", "description": "The subagent to query. Omit to report every tracked subagent." } },
            "required": [], "additionalProperties": false
        }
    })
}

/// Keep the first cap bytes without splitting a UTF-8 codepoint.
pub fn truncate_output(bytes: &[u8]) -> (String, bool) {
    if bytes.len() <= SUBAGENT_OUTPUT_CAP_BYTES {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut end = SUBAGENT_OUTPUT_CAP_BYTES;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    (String::from_utf8_lossy(&bytes[..end]).into_owned(), true)
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Substitute exactly the validated placeholders in one pass.
pub fn interpolate(template: &str, model: &str, prompt: &str, prompt_via: &str) -> String {
    let quoted_model = shell_quote(model);
    let quoted_prompt = if prompt_via == "arg" {
        shell_quote(prompt)
    } else {
        String::new()
    };

    // Walk the validated template once. In particular, never scan substituted
    // text again: an untrusted model value containing `{prompt}` must remain
    // data inside its shell-quoted argument, not become a second placeholder.
    let mut output = String::with_capacity(template.len() + quoted_model.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        output.push_str(&rest[..open]);
        rest = &rest[open..];
        if let Some(tail) = rest.strip_prefix("{model}") {
            output.push_str(&quoted_model);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("{prompt}") {
            output.push_str(&quoted_prompt);
            rest = tail;
        } else {
            // Open/join validation guarantees this is unreachable. Preserve
            // the byte rather than dropping data if the helper is called alone.
            output.push('{');
            rest = &rest[1..];
        }
    }
    output.push_str(rest);
    output
}

pub fn timeout_for(def: &SubagentDef, default: Duration) -> Duration {
    Duration::from_secs(def.timeout_secs.unwrap_or(default.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_never_reinterprets_substituted_placeholders() {
        let model = "x'{prompt}";
        let prompt = ";touch /pwned;";
        let command = interpolate("cli --model {model} --print {prompt}", model, prompt, "arg");

        assert_eq!(
            command,
            "cli --model 'x'\\''{prompt}' --print ';touch /pwned;'"
        );
        assert_eq!(command.matches(";touch /pwned;").count(), 1);
    }

    #[test]
    fn stdin_interpolation_preserves_placeholder_text_inside_model() {
        let command = interpolate("cli --model {model}", "literal {prompt}", "secret", "stdin");
        assert_eq!(command, "cli --model 'literal {prompt}'");
        assert!(!command.contains("secret"));
    }
}
