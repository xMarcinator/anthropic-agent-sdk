//! Message types for conversations

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::identifiers::SessionId;
use super::introspection::{ModelUsage, SDKPermissionDenial};

// ============================================================================
// AskUserQuestion Tool Types
// ============================================================================

/// Option for a question in `AskUserQuestion`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Display text for this option
    pub label: String,
    /// Explanation of what this option means
    pub description: String,
}

/// Question specification for `AskUserQuestion`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionSpec {
    /// The complete question to ask the user
    pub question: String,
    /// Short label displayed as a chip/tag (max 12 chars)
    pub header: String,
    /// Available choices (2-4 options)
    pub options: Vec<QuestionOption>,
    /// Whether multiple options can be selected
    pub multi_select: bool,
}

/// Input for the `AskUserQuestion` tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserQuestionInput {
    /// Questions to ask the user (1-4 questions)
    pub questions: Vec<QuestionSpec>,
    /// User answers collected (populated when tool result returns)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<HashMap<String, String>>,
}

/// Output from the `AskUserQuestion` tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserQuestionOutput {
    /// User's answers keyed by question header
    pub answers: HashMap<String, String>,
}

// ============================================================================
// Message Types
// ============================================================================

/// Content value for tool results
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContentValue {
    /// String content
    String(String),
    /// Structured content blocks
    Blocks(Vec<serde_json::Value>),
}

/// Content block types
///
/// Block types this SDK does not model (`redacted_thinking`, `server_tool_use`, `image`, ...)
/// parse as [`ContentBlock::Other`] with their raw JSON, so a new block type never fails the
/// whole message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    /// Text content block
    Text {
        /// Text content
        text: String,
    },
    /// Thinking content block (extended thinking)
    Thinking {
        /// Thinking content
        thinking: String,
        /// Signature for verification (empty when the CLI omits it)
        #[serde(default)]
        signature: String,
    },
    /// Tool use request
    ToolUse {
        /// Tool use ID
        id: String,
        /// Tool name
        name: String,
        /// Tool input parameters
        input: serde_json::Value,
    },
    /// Tool execution result
    ToolResult {
        /// ID of the tool use this is a result for
        tool_use_id: String,
        /// Result content
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ContentValue>,
        /// Whether this is an error result
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Any other block, as raw JSON (including its `type`)
    #[serde(untagged)]
    Other(serde_json::Value),
}

/// User message content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessageContent {
    /// Message role (always "user")
    pub role: String,
    /// Message content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<UserContent>,
}

/// User content can be string or blocks
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    /// Plain string content
    String(String),
    /// Structured content blocks
    Blocks(Vec<ContentBlock>),
}

/// Assistant message content
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantMessageContent {
    /// Model that generated the message (`<synthetic>` for CLI-made messages such as auth
    /// errors; empty when absent)
    #[serde(default)]
    pub model: String,
    /// Message content blocks
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// API message ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Why the model stopped (`end_turn`, `tool_use`, ...); null while streaming
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Token usage of this API call (not final on streamed partial messages)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
}

impl AssistantMessageContent {
    /// Content from `model` with `content` and no other fields.
    #[must_use]
    pub fn new(model: impl Into<String>, content: Vec<ContentBlock>) -> Self {
        Self {
            model: model.into(),
            content,
            ..Self::default()
        }
    }
}

/// Token usage of a turn, as in the `usage` object of `result` and assistant messages.
///
/// Unknown fields (`server_tool_use`, `cache_creation`, `service_tier`, ...) are kept in
/// `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens not read from or written to the cache
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens
    #[serde(default)]
    pub output_tokens: u64,
    /// Input tokens written to the cache
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Input tokens read from the cache
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Every other field
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Which usage limit a [`RateLimitInfo`] is about.
///
/// Claude Code 2.1.292 sends `five_hour`, `seven_day`, `seven_day_opus`, `seven_day_sonnet`,
/// `seven_day_overage_included` and `overage`; any other value is kept in `Other`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RateLimitType {
    /// The rolling five-hour session window
    FiveHour,
    /// The weekly window across all models
    SevenDay,
    /// The weekly Opus window
    SevenDayOpus,
    /// The weekly Sonnet window
    SevenDaySonnet,
    /// The weekly window including overage
    SevenDayOverageIncluded,
    /// Paid overage
    Overage,
    /// A window this SDK does not know yet
    Other(String),
}

impl RateLimitType {
    /// The wire name of the window.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::FiveHour => "five_hour",
            Self::SevenDay => "seven_day",
            Self::SevenDayOpus => "seven_day_opus",
            Self::SevenDaySonnet => "seven_day_sonnet",
            Self::SevenDayOverageIncluded => "seven_day_overage_included",
            Self::Overage => "overage",
            Self::Other(other) => other,
        }
    }
}

impl From<&str> for RateLimitType {
    fn from(value: &str) -> Self {
        match value {
            "five_hour" => Self::FiveHour,
            "seven_day" => Self::SevenDay,
            "seven_day_opus" => Self::SevenDayOpus,
            "seven_day_sonnet" => Self::SevenDaySonnet,
            "seven_day_overage_included" => Self::SevenDayOverageIncluded,
            "overage" => Self::Overage,
            other => Self::Other(other.to_string()),
        }
    }
}

impl Serialize for RateLimitType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RateLimitType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from(value.as_str()))
    }
}

/// One usage window's state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// Fraction or percentage used, as the CLI reports it
    #[serde(default)]
    pub utilization: Option<f64>,
    /// When the window resets (Unix seconds)
    #[serde(default)]
    pub resets_at: Option<i64>,
    /// Every other field
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// The `rate_limit_info` of a `rate_limit_event`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    /// `allowed`, `allowed_warning` or `rejected`
    #[serde(default)]
    pub status: String,
    /// When the limit named by `rate_limit_type` resets (Unix seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// The window this event is about
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_type: Option<RateLimitType>,
    /// Utilization of that window
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// Per-window usage (`five_hour`, `seven_day`, `seven_day_overage_included`, ...), keyed by
    /// window name. Marked internal by Claude Code, so it may be absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified_windows: Option<HashMap<String, RateLimitWindow>>,
    /// Overage status, when overage applies
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_status: Option<String>,
    /// When overage resets (Unix seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_resets_at: Option<i64>,
    /// Whether the request used overage
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_using_overage: Option<bool>,
    /// Every other field
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl RateLimitInfo {
    /// Whether the request was refused.
    #[must_use]
    pub fn is_rejected(&self) -> bool {
        self.status == "rejected"
    }

    /// The state of one window: from `unified_windows`, or from the top-level fields when the
    /// event is about that window.
    #[must_use]
    pub fn window(&self, window: &RateLimitType) -> Option<RateLimitWindow> {
        if let Some(found) = self
            .unified_windows
            .as_ref()
            .and_then(|windows| windows.get(window.as_str()))
        {
            return Some(found.clone());
        }
        (self.rate_limit_type.as_ref() == Some(window)).then(|| RateLimitWindow {
            utilization: self.utilization,
            resets_at: self.resets_at,
            extra: serde_json::Map::new(),
        })
    }
}

/// The `system`/`init` message, typed. Unknown fields stay in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemInit {
    /// Conversation ID
    #[serde(default)]
    pub session_id: Option<String>,
    /// Working directory
    #[serde(default)]
    pub cwd: Option<String>,
    /// Resolved model ID (e.g. `claude-haiku-4-5-20251001`)
    #[serde(default)]
    pub model: Option<String>,
    /// Permission mode as the CLI reports it (`default`, `plan`, ...)
    #[serde(default, rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// Where the API key came from; `none` means a Claude account login (subscription)
    #[serde(default, rename = "apiKeySource")]
    pub api_key_source: Option<String>,
    /// Claude Code's version (e.g. `2.1.292`)
    #[serde(default)]
    pub claude_code_version: Option<String>,
    /// Tool names
    #[serde(default)]
    pub tools: Vec<String>,
    /// MCP servers and their status
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
    /// Slash commands
    #[serde(default)]
    pub slash_commands: Vec<String>,
    /// Agent names
    #[serde(default)]
    pub agents: Vec<String>,
    /// Skill names
    #[serde(default)]
    pub skills: Vec<String>,
    /// Loaded plugins
    #[serde(default)]
    pub plugins: Vec<serde_json::Value>,
    /// Output style
    #[serde(default)]
    pub output_style: Option<String>,
    /// Protocol capabilities, e.g. `interrupt_receipt_v1`, `msg_lifecycle_v1`
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Message UUID
    #[serde(default)]
    pub uuid: Option<String>,
    /// Every other field
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SystemInit {
    /// Whether the session is logged in with a Claude account rather than an API key.
    #[must_use]
    pub fn is_subscription(&self) -> bool {
        self.api_key_source.as_deref() == Some("none")
    }

    /// Whether the CLI advertised a protocol capability.
    #[must_use]
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// Message types
///
/// Message types this SDK does not model (`tool_progress`, `auth_status`,
/// `prompt_suggestion`, ...) parse as [`Message::Other`] with their raw JSON instead of failing,
/// as does a known type whose payload no longer matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Message {
    /// User message
    User {
        /// Parent tool use ID for nested conversations
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        /// Message content
        message: UserMessageContent,
        /// Session ID
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        /// Message UUID: the checkpoint for file rewind, and the `uuid` the SDK sent when this
        /// is a replay (`--replay-user-messages`)
        #[serde(skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        /// Whether this is the echo of a message written to stdin (`isReplay`)
        #[serde(
            rename = "isReplay",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        is_replay: bool,
        /// Whether the CLI made this message rather than the user (`isSynthetic`)
        #[serde(
            rename = "isSynthetic",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        is_synthetic: bool,
        /// When the message was taken (ISO 8601)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    /// Assistant message
    Assistant {
        /// Parent tool use ID for nested conversations
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        /// Message content
        message: AssistantMessageContent,
        /// Session ID
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
        /// Message UUID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        /// Error kind of a CLI-made error message, e.g. `authentication_failed`,
        /// `rate_limit`, `billing_error`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// System message (`init`, `status`, `compact_boundary`, `api_retry`, ...). Use
    /// [`Message::as_system_init`] for a typed `init`.
    System {
        /// System message subtype
        subtype: String,
        /// Additional system message data
        #[serde(flatten)]
        data: serde_json::Value,
    },
    /// Result message with metrics
    Result {
        /// Result subtype (success, `error_max_turns`, `error_during_execution`, etc.)
        subtype: String,
        /// Total duration in milliseconds
        #[serde(default)]
        duration_ms: u64,
        /// API call duration in milliseconds
        #[serde(default)]
        duration_api_ms: u64,
        /// Whether this is an error result
        #[serde(default)]
        is_error: bool,
        /// Number of conversation turns
        #[serde(default)]
        num_turns: u32,
        /// Session ID
        #[serde(default)]
        session_id: SessionId,
        /// Total cost in USD
        #[serde(skip_serializing_if = "Option::is_none")]
        total_cost_usd: Option<f64>,
        /// Token usage statistics of the turn's main agent; see [`Message::result_usage`]
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<serde_json::Value>,
        /// Result message (for success subtype)
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Per-model usage statistics (subagents included)
        #[serde(
            rename = "modelUsage",
            default,
            skip_serializing_if = "HashMap::is_empty"
        )]
        model_usage: HashMap<String, ModelUsage>,
        /// List of denied tool uses
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        permission_denials: Vec<SDKPermissionDenial>,
        /// Structured output (when outputFormat is specified)
        #[serde(skip_serializing_if = "Option::is_none")]
        structured_output: Option<serde_json::Value>,
        /// Error messages (for error subtypes)
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        errors: Vec<String>,
        /// Message UUID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        /// Why the turn's last API call stopped
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        /// Why the query loop ended (`completed`, `api_error`, `aborted`, ...)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_reason: Option<String>,
        /// The user messages this result answers (their `uuid`s)
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        user_message_uuids: Vec<String>,
        /// Every other field (`fast_mode_state`, `subagent_stats`, `api_error_status`, ...)
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    /// Stream event for partial messages
    StreamEvent {
        /// Event UUID
        uuid: String,
        /// Session ID
        session_id: SessionId,
        /// Raw stream event data
        event: serde_json::Value,
        /// Parent tool use ID
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    /// Rate limit status, emitted when it changes
    RateLimitEvent {
        /// The limit's state
        rate_limit_info: RateLimitInfo,
        /// Message UUID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        /// Session ID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// The progress of one user message written to stdin: `queued`, `started`, `cancelled`, ...
    /// (capability `msg_lifecycle_v1`)
    CommandLifecycle {
        /// The `uuid` of the user message
        command_uuid: String,
        /// Its new state
        state: String,
        /// Message UUID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        /// Session ID
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<SessionId>,
    },
    /// A control request from the CLI (`can_use_tool`, `hook_callback`, `mcp_message`, ...).
    /// `ClaudeSDKClient` answers these itself and does not yield them.
    ControlRequest {
        /// Correlates the answer
        request_id: String,
        /// The request
        request: crate::control::wire::ControlRequestBody,
    },
    /// The CLI's answer to a control request. `ClaudeSDKClient` routes these to the waiting
    /// caller and does not yield them.
    ControlResponse {
        /// The answer
        response: crate::control::wire::ControlResponseBody,
    },
    /// The CLI withdrawing one of its control requests
    ControlCancelRequest {
        /// The withdrawn request
        request_id: String,
    },
    /// Any other message, as raw JSON (including its `type`)
    #[serde(untagged)]
    Other(serde_json::Value),
}

impl Message {
    /// The message's `type` as it appears on the wire.
    #[must_use]
    pub fn message_type(&self) -> &str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::System { .. } => "system",
            Self::Result { .. } => "result",
            Self::StreamEvent { .. } => "stream_event",
            Self::RateLimitEvent { .. } => "rate_limit_event",
            Self::CommandLifecycle { .. } => "command_lifecycle",
            Self::ControlRequest { .. } => "control_request",
            Self::ControlResponse { .. } => "control_response",
            Self::ControlCancelRequest { .. } => "control_cancel_request",
            Self::Other(value) => value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
        }
    }

    /// The typed `system`/`init` message, if this is one.
    #[must_use]
    pub fn as_system_init(&self) -> Option<SystemInit> {
        match self {
            Self::System { subtype, data } if subtype == "init" => {
                serde_json::from_value(data.clone()).ok()
            }
            _ => None,
        }
    }

    /// The typed `usage` of a result message.
    #[must_use]
    pub fn result_usage(&self) -> Option<Usage> {
        match self {
            Self::Result {
                usage: Some(usage), ..
            } => serde_json::from_value(usage.clone()).ok(),
            _ => None,
        }
    }

    /// Whether this is the CLI's echo of a user message written to stdin.
    #[must_use]
    pub fn is_replay(&self) -> bool {
        matches!(
            self,
            Self::User {
                is_replay: true,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_question_option_serde() {
        let opt = QuestionOption {
            label: "Option A".to_string(),
            description: "First option".to_string(),
        };

        let json = serde_json::to_string(&opt).unwrap();
        assert!(json.contains("Option A"));
        assert!(json.contains("First option"));

        let parsed: QuestionOption = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.label, "Option A");
        assert_eq!(parsed.description, "First option");
    }

    #[test]
    fn test_question_spec_serde() {
        let spec = QuestionSpec {
            question: "Which approach?".to_string(),
            header: "Approach".to_string(),
            options: vec![
                QuestionOption {
                    label: "A".to_string(),
                    description: "Option A".to_string(),
                },
                QuestionOption {
                    label: "B".to_string(),
                    description: "Option B".to_string(),
                },
            ],
            multi_select: false,
        };

        let json = serde_json::to_string(&spec).unwrap();
        // camelCase for multiSelect
        assert!(json.contains("multiSelect"));
        assert!(!json.contains("multi_select"));

        let parsed: QuestionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.question, "Which approach?");
        assert_eq!(parsed.header, "Approach");
        assert_eq!(parsed.options.len(), 2);
        assert!(!parsed.multi_select);
    }

    #[test]
    fn test_question_spec_multi_select() {
        let spec = QuestionSpec {
            question: "Select features".to_string(),
            header: "Features".to_string(),
            options: vec![
                QuestionOption {
                    label: "Feature 1".to_string(),
                    description: "First feature".to_string(),
                },
                QuestionOption {
                    label: "Feature 2".to_string(),
                    description: "Second feature".to_string(),
                },
            ],
            multi_select: true,
        };

        let json = serde_json::to_string(&spec).unwrap();
        let parsed: QuestionSpec = serde_json::from_str(&json).unwrap();
        assert!(parsed.multi_select);
    }

    #[test]
    fn test_ask_user_question_input_serde() {
        let input = AskUserQuestionInput {
            questions: vec![QuestionSpec {
                question: "Which library?".to_string(),
                header: "Library".to_string(),
                options: vec![
                    QuestionOption {
                        label: "tokio".to_string(),
                        description: "Async runtime".to_string(),
                    },
                    QuestionOption {
                        label: "async-std".to_string(),
                        description: "Alternative runtime".to_string(),
                    },
                ],
                multi_select: false,
            }],
            answers: None,
        };

        let json = serde_json::to_string(&input).unwrap();
        // answers should be omitted when None
        assert!(!json.contains("answers"));

        let parsed: AskUserQuestionInput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.questions.len(), 1);
        assert!(parsed.answers.is_none());
    }

    #[test]
    fn test_ask_user_question_input_with_answers() {
        let mut answers = HashMap::new();
        answers.insert("Library".to_string(), "tokio".to_string());

        let input = AskUserQuestionInput {
            questions: vec![QuestionSpec {
                question: "Which library?".to_string(),
                header: "Library".to_string(),
                options: vec![QuestionOption {
                    label: "tokio".to_string(),
                    description: "Async runtime".to_string(),
                }],
                multi_select: false,
            }],
            answers: Some(answers),
        };

        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("answers"));
        assert!(json.contains("tokio"));

        let parsed: AskUserQuestionInput = serde_json::from_str(&json).unwrap();
        assert!(parsed.answers.is_some());
        assert_eq!(
            parsed.answers.as_ref().unwrap().get("Library"),
            Some(&"tokio".to_string())
        );
    }

    #[test]
    fn test_ask_user_question_output_serde() {
        let mut answers = HashMap::new();
        answers.insert("Library".to_string(), "tokio".to_string());
        answers.insert("Framework".to_string(), "axum".to_string());

        let output = AskUserQuestionOutput { answers };

        let json = serde_json::to_string(&output).unwrap();
        let parsed: AskUserQuestionOutput = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.answers.len(), 2);
        assert_eq!(parsed.answers.get("Library"), Some(&"tokio".to_string()));
        assert_eq!(parsed.answers.get("Framework"), Some(&"axum".to_string()));
    }

    #[test]
    fn test_ask_user_question_from_json_value() {
        // Simulate parsing from a ToolUse input
        let json_value = serde_json::json!({
            "questions": [{
                "question": "Which database?",
                "header": "Database",
                "options": [
                    {"label": "PostgreSQL", "description": "Relational DB"},
                    {"label": "MongoDB", "description": "Document DB"}
                ],
                "multiSelect": false
            }]
        });

        let input: AskUserQuestionInput = serde_json::from_value(json_value).unwrap();
        assert_eq!(input.questions.len(), 1);
        assert_eq!(input.questions[0].header, "Database");
        assert_eq!(input.questions[0].options.len(), 2);
    }
}
