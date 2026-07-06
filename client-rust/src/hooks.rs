//! Optional lifecycle hooks for the harness loop.
//!
//! Four points let a program observe or mutate the round-trip:
//!
//! - [`Hooks::before_send`] — the outgoing [`Message`] just before it is POSTed.
//! - [`Hooks::after_receive`] — the assistant [`Message`] just after it arrives.
//! - [`Hooks::before_tool_call`] — a [`ToolUse`] just before its handler runs.
//! - [`Hooks::after_tool_call`] — a [`ToolResult`] just before it is sent back.
//!
//! Each hook receives `&mut` access to its event (so it may rewrite it) and
//! returns [`HookResult`]. Returning `Err` aborts the loop: the harness stops
//! immediately and surfaces the error as [`crate::Error::Hook`].

use crate::tool::BoxError;
use crate::types::{Message, ToolResult, ToolUse};

/// Result type for hook callbacks. `Err` aborts the harness loop.
pub type HookResult = Result<(), BoxError>;

type MessageHook = Box<dyn FnMut(&mut Message) -> HookResult + Send>;
type ToolUseHook = Box<dyn FnMut(&mut ToolUse) -> HookResult + Send>;
type ToolResultHook = Box<dyn FnMut(&mut ToolResult) -> HookResult + Send>;

/// A set of optional lifecycle callbacks. Construct with [`Hooks::default`]
/// and attach callbacks with the builder methods; unset hooks are no-ops.
#[derive(Default)]
pub struct Hooks {
    before_send: Option<MessageHook>,
    after_receive: Option<MessageHook>,
    before_tool_call: Option<ToolUseHook>,
    after_tool_call: Option<ToolResultHook>,
}

impl Hooks {
    /// Runs before each outgoing message is sent to the server.
    pub fn before_send<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut Message) -> HookResult + Send + 'static,
    {
        self.before_send = Some(Box::new(f));
        self
    }

    /// Runs after each assistant message is received from the server.
    pub fn after_receive<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut Message) -> HookResult + Send + 'static,
    {
        self.after_receive = Some(Box::new(f));
        self
    }

    /// Runs before each client-side tool handler is invoked.
    pub fn before_tool_call<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut ToolUse) -> HookResult + Send + 'static,
    {
        self.before_tool_call = Some(Box::new(f));
        self
    }

    /// Runs after each tool handler returns, before the result is sent back.
    pub fn after_tool_call<F>(mut self, f: F) -> Self
    where
        F: FnMut(&mut ToolResult) -> HookResult + Send + 'static,
    {
        self.after_tool_call = Some(Box::new(f));
        self
    }

    pub(crate) fn run_before_send(&mut self, msg: &mut Message) -> HookResult {
        match &mut self.before_send {
            Some(f) => f(msg),
            None => Ok(()),
        }
    }

    pub(crate) fn run_after_receive(&mut self, msg: &mut Message) -> HookResult {
        match &mut self.after_receive {
            Some(f) => f(msg),
            None => Ok(()),
        }
    }

    pub(crate) fn run_before_tool_call(&mut self, call: &mut ToolUse) -> HookResult {
        match &mut self.before_tool_call {
            Some(f) => f(call),
            None => Ok(()),
        }
    }

    pub(crate) fn run_after_tool_call(&mut self, result: &mut ToolResult) -> HookResult {
        match &mut self.after_tool_call {
            Some(f) => f(result),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn mark<T>(o: &Option<T>) -> &'static str {
            if o.is_some() {
                "set"
            } else {
                "unset"
            }
        }
        f.debug_struct("Hooks")
            .field("before_send", &mark(&self.before_send))
            .field("after_receive", &mark(&self.after_receive))
            .field("before_tool_call", &mark(&self.before_tool_call))
            .field("after_tool_call", &mark(&self.after_tool_call))
            .finish()
    }
}
