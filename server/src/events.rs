//! The message-type schema.
//!
//! [`EventType`] is the **closed** set of `event_type` strings that may appear
//! in the `session_events` table. It is a plain enum with an exhaustive
//! [`EventType::as_str`] match, so adding a new variant without also giving it a
//! wire string is a compile error — new message types can never be silently
//! unhandled.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Every event type that can be recorded in `session_events`.
///
/// The `serde` rename on each variant is the exact wire/storage string; keep it
/// in sync with [`EventType::as_str`] (the `#[deny]`-free compiler already forces
/// the match below to stay exhaustive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    /// Client sends a user turn to the server.
    #[serde(rename = "client.message.send")]
    ClientMessageSend,
    /// Server sends the LLM's response back to the client.
    #[serde(rename = "server.message.send")]
    ServerMessageSend,
    /// Full request payload about to be sent to the LLM provider.
    #[serde(rename = "provider.request")]
    ProviderRequest,
    /// Raw response received from the LLM provider.
    #[serde(rename = "provider.response")]
    ProviderResponse,
    /// Server or harness invokes a tool (client-side or MCP).
    #[serde(rename = "tool.call")]
    ToolCall,
    /// Result returned from a tool call.
    #[serde(rename = "tool.result")]
    ToolResult,
    /// Request sent to an MCP server (real `tools/call` dispatch).
    #[serde(rename = "mcp.request")]
    McpRequest,
    /// Response from an MCP server.
    #[serde(rename = "mcp.response")]
    McpResponse,
    /// Session connection established.
    #[serde(rename = "session.open")]
    SessionOpen,
    /// Session connection closed normally.
    #[serde(rename = "session.close")]
    SessionClose,
    /// Session terminated due to error.
    #[serde(rename = "session.error")]
    SessionError,
    /// Session history was compacted (summary event).
    #[serde(rename = "session.compaction")]
    SessionCompaction,
}

impl EventType {
    /// Every variant, in definition order. Handy for tests and documentation.
    pub const ALL: [EventType; 12] = [
        EventType::ClientMessageSend,
        EventType::ServerMessageSend,
        EventType::ProviderRequest,
        EventType::ProviderResponse,
        EventType::ToolCall,
        EventType::ToolResult,
        EventType::McpRequest,
        EventType::McpResponse,
        EventType::SessionOpen,
        EventType::SessionClose,
        EventType::SessionError,
        EventType::SessionCompaction,
    ];

    /// The canonical wire/storage string for this event type.
    ///
    /// This match is exhaustive on purpose: adding a new variant without a
    /// string here fails to compile.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::ClientMessageSend => "client.message.send",
            EventType::ServerMessageSend => "server.message.send",
            EventType::ProviderRequest => "provider.request",
            EventType::ProviderResponse => "provider.response",
            EventType::ToolCall => "tool.call",
            EventType::ToolResult => "tool.result",
            EventType::McpRequest => "mcp.request",
            EventType::McpResponse => "mcp.response",
            EventType::SessionOpen => "session.open",
            EventType::SessionClose => "session.close",
            EventType::SessionError => "session.error",
            EventType::SessionCompaction => "session.compaction",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a string does not name a known [`EventType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownEventType(pub String);

impl fmt::Display for UnknownEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown event_type: {:?}", self.0)
    }
}

impl std::error::Error for UnknownEventType {}

impl FromStr for EventType {
    type Err = UnknownEventType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        EventType::ALL
            .into_iter()
            .find(|ev| ev.as_str() == s)
            .ok_or_else(|| UnknownEventType(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants_have_distinct_strings() {
        let mut seen = std::collections::HashSet::new();
        for ev in EventType::ALL {
            assert!(seen.insert(ev.as_str()), "duplicate wire string: {ev}");
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn string_round_trip() {
        for ev in EventType::ALL {
            assert_eq!(ev.as_str().parse::<EventType>().unwrap(), ev);
        }
    }

    #[test]
    fn serde_uses_wire_strings() {
        let json = serde_json::to_string(&EventType::ClientMessageSend).unwrap();
        assert_eq!(json, "\"client.message.send\"");
        let back: EventType = serde_json::from_str("\"session.compaction\"").unwrap();
        assert_eq!(back, EventType::SessionCompaction);
    }

    #[test]
    fn unknown_string_is_rejected() {
        assert!("not.a.real.type".parse::<EventType>().is_err());
    }
}
