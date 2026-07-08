//! Client-side tool definitions.
//!
//! A [`Tool`] pairs the schema the server needs (name, description, JSON input
//! schema) with an **async** handler the harness invokes when the model asks
//! for it. Handlers are `Fn` (not `FnMut`) so a single tool can be dispatched
//! concurrently and safely across turns; each invocation returns a boxed
//! future so a handler can `.await` (an HTTP round-trip, a subprocess) without
//! blocking the runtime — see [`crate::sandbox`], whose remote-sandbox tools
//! make a `session.execRemoteSandbox` call from inside their handler.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

/// Boxed error type returned by tool handlers and hooks.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The future a [`ToolHandler`] returns: resolves to the result content
/// (string or blocks, as raw JSON) or an error that aborts the harness loop.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, BoxError>> + Send>>;

/// The callable behind a [`Tool`]: takes the model-supplied `input` JSON and
/// returns a [`ToolFuture`]. Async so a handler can perform I/O (e.g. a remote
/// sandbox RPC) without blocking the async runtime.
pub type ToolHandler = Box<dyn Fn(Value) -> ToolFuture + Send + Sync>;

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
    /// Define a tool from its metadata and an **async** handler closure.
    ///
    /// The handler returns a future; a synchronous body is expressed with an
    /// `async move` block. This async shape is what lets sandbox tools make a
    /// network round-trip from inside the handler (see [`crate::sandbox`]).
    ///
    /// ```
    /// use bae_rs::Tool;
    /// use serde_json::json;
    ///
    /// let t = Tool::new(
    ///     "echo",
    ///     "Echo the input back",
    ///     json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
    ///     |input| async move { Ok(input) },
    /// );
    /// assert_eq!(t.name, "echo");
    /// ```
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, BoxError>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler: Box::new(move |input| Box::pin(handler(input))),
        }
    }

    /// Define a tool from a pre-boxed [`ToolHandler`]. Used by builtin tool
    /// constructors (e.g. [`crate::sandbox`]) that assemble the handler
    /// themselves rather than via a bare closure.
    pub fn from_handler(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: ToolHandler,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler,
        }
    }

    /// Invoke the handler with the given input, awaiting its future.
    pub(crate) async fn call(&self, input: Value) -> Result<Value, BoxError> {
        (self.handler)(input).await
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
