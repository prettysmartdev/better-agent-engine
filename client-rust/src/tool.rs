//! Client-side tool definitions.
//!
//! A [`Tool`] pairs the schema the server needs (name, description, JSON input
//! schema) with a synchronous handler the harness invokes when the model asks
//! for it. Handlers are `Fn` (not `FnMut`) so a single tool can be dispatched
//! concurrently and safely across turns.

use serde_json::Value;

/// Boxed error type returned by tool handlers and hooks.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The callable behind a [`Tool`]: takes the model-supplied `input` JSON and
/// returns the result content (string or blocks, as raw JSON) or an error that
/// aborts the harness loop.
pub type ToolHandler = Box<dyn Fn(Value) -> Result<Value, BoxError> + Send + Sync>;

/// A client-side tool the harness can execute on the model's behalf.
pub struct Tool {
    /// Unique tool name; must appear in the profile's `allowed_tools`.
    pub name: String,
    /// Human-/model-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub input_schema: Value,
    handler: ToolHandler,
}

impl Tool {
    /// Define a tool from its metadata and a handler closure.
    ///
    /// ```
    /// use bae_rs::Tool;
    /// use serde_json::json;
    ///
    /// let t = Tool::new(
    ///     "echo",
    ///     "Echo the input back",
    ///     json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
    ///     |input| Ok(input),
    /// );
    /// assert_eq!(t.name, "echo");
    /// ```
    pub fn new<F>(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value) -> Result<Value, BoxError> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: Box::new(handler),
        }
    }

    /// Invoke the handler with the given input.
    pub(crate) fn call(&self, input: Value) -> Result<Value, BoxError> {
        (self.handler)(input)
    }

    /// The declaration the server expects in `POST /api/v1/sessions`.
    pub(crate) fn declaration(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        })
    }
}

impl std::fmt::Debug for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("handler", &"<fn>")
            .finish()
    }
}
