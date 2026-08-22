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
    /// Assistant hidden reasoning (Anthropic `thinking` block, OAI
    /// `reasoning_content`). Kept so tool-loop replays stay lossless: Anthropic
    /// requires the thinking block — including its opaque `signature`, when the
    /// upstream enforces handoff — to be sent back verbatim on subsequent turns.
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
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

/// Assistant hidden reasoning captured from an upstream stream.
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    /// The reasoning text accumulated this turn.
    pub thinking: String,
    /// Anthropic handoff token; `None` for chat upstreams (e.g. `reasoning_content`).
    pub signature: Option<String>,
}

/// One message in the conversation history sent to the upstream API.
///
/// Serializable so the session log can persist each message as a JSONL
/// `message` event and `--context` can replay them losslessly.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// A single streamed token, tagged by its kind so stdout can label
/// thinking (reasoning) separately from the final answer text.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Reasoning / thinking fragment (anthropic `thinking_delta`, OAI
    /// `reasoning_content`). Never fed back as assistant content.
    Think(String),
    /// Answer text fragment (the model's visible reply).
    Answer(String),
}

/// Exposed by each upstream client: the tool calls (if any) and the full
/// assistant text produced this turn.
#[derive(Debug, Default)]
pub struct StreamOutcome {
    pub tool_calls: Vec<ToolCall>,
    pub assistant_text: String,
    /// Hidden reasoning produced this turn, when the upstream emits any.
    /// Replayed in the assistant turn so multi-turn tool loops stay lossless
    /// (Anthropic requires the `signature` back on continuation).
    pub assistant_thinking: Option<ThinkingBlock>,
}
