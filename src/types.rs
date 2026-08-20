//! Neutral message & tool types shared across the crate.
//!
//! The turn loop talks exclusively in these types; each upstream client
//! (`anthropic`, `oai_chat`) converts them to/from its own wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Conversation role (system is carried separately in [`crate::persona`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// A single content block inside a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    /// Plain assistant/user text.
    Text(String),
    /// Assistant request to invoke a tool. Also the shape we hand to upstream
    /// clients as the assistant `tool_use`.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a previously-requested [`ContentBlock::ToolUse`].
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// One message in the conversation history sent to the upstream API.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text(text.into())],
        }
    }
}

/// A tool definition exposed to the model.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool arguments.
    pub input_schema: Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Exposed by each upstream client: the tool calls (if any) and the full
/// assistant text produced this turn.
#[derive(Debug, Default)]
pub struct StreamOutcome {
    pub tool_calls: Vec<ToolCall>,
    pub assistant_text: String,
}
