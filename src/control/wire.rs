//! The stream-JSON control protocol as Claude Code speaks it on stdin/stdout.
//!
//! Every control message is one JSON line:
//!
//! ```text
//! SDK -> CLI  {"type":"control_request","request_id":"r1","request":{"subtype":"interrupt"}}
//! CLI -> SDK  {"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"still_queued":[]}}}
//! CLI -> SDK  {"type":"control_request","request_id":"c1","request":{"subtype":"can_use_tool","tool_name":"Bash",...}}
//! SDK -> CLI  {"type":"control_response","response":{"subtype":"success","request_id":"c1","response":{"behavior":"allow",...}}}
//! ```
//!
//! Requests and responses keep every field they carry: a request body is its `subtype` plus a
//! JSON map, so subtypes and fields added by newer Claude Code versions survive a round trip.
//! Verified against Claude Code 2.1.292.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::types::{PermissionMode, PermissionResult};

/// Subtype of the request that stops the current turn.
pub const INTERRUPT: &str = "interrupt";
/// Subtype of the handshake request that returns commands, models and account data.
pub const INITIALIZE: &str = "initialize";
/// Subtype of the request that switches the model mid-session.
pub const SET_MODEL: &str = "set_model";
/// Subtype of the request that switches the permission mode mid-session.
pub const SET_PERMISSION_MODE: &str = "set_permission_mode";
/// Subtype of the request that changes the thinking budget mid-session.
pub const SET_MAX_THINKING_TOKENS: &str = "set_max_thinking_tokens";
/// Subtype of the request that restores files to a user message's checkpoint.
pub const REWIND_FILES: &str = "rewind_files";
/// Subtype of the request that lists MCP server status.
pub const MCP_STATUS: &str = "mcp_status";
/// Subtype of the CLI's request asking whether a tool may run (`--permission-prompt-tool stdio`).
pub const CAN_USE_TOOL: &str = "can_use_tool";
/// Subtype of the CLI's request invoking an SDK-registered hook.
pub const HOOK_CALLBACK: &str = "hook_callback";
/// Subtype of the CLI's request carrying a JSON-RPC message for an SDK MCP server.
pub const MCP_MESSAGE: &str = "mcp_message";

/// The `request` object of a `control_request`: a subtype and its fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequestBody {
    /// The request kind, e.g. `interrupt` or `can_use_tool`.
    pub subtype: String,
    /// Every other field of the request.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl ControlRequestBody {
    /// A request of `subtype` with no fields.
    #[must_use]
    pub fn new(subtype: impl Into<String>) -> Self {
        Self {
            subtype: subtype.into(),
            fields: Map::new(),
        }
    }

    /// Adds or replaces one field.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// `interrupt`: stop the current turn. The CLI answers with `{"still_queued": [...]}`.
    #[must_use]
    pub fn interrupt() -> Self {
        Self::new(INTERRUPT)
    }

    /// `initialize`: the handshake that returns commands, agents, models and account data.
    #[must_use]
    pub fn initialize() -> Self {
        Self::new(INITIALIZE)
    }

    /// `set_model`: switch the model for later turns; `None` returns to the default.
    #[must_use]
    pub fn set_model(model: Option<String>) -> Self {
        Self::new(SET_MODEL).with("model", model.map_or(Value::Null, Value::String))
    }

    /// `set_permission_mode`.
    #[must_use]
    pub fn set_permission_mode(mode: PermissionMode) -> Self {
        Self::new(SET_PERMISSION_MODE).with("mode", mode.control_value())
    }

    /// `set_max_thinking_tokens`; `None` turns the budget off.
    #[must_use]
    pub fn set_max_thinking_tokens(tokens: Option<u32>) -> Self {
        Self::new(SET_MAX_THINKING_TOKENS).with(
            "max_thinking_tokens",
            tokens.map_or(Value::Null, Value::from),
        )
    }

    /// `rewind_files` to the checkpoint of a user message. Both spellings of the id field are
    /// sent, since the CLI has used each.
    #[must_use]
    pub fn rewind_files(user_message_uuid: impl Into<String>) -> Self {
        let id = user_message_uuid.into();
        Self::new(REWIND_FILES)
            .with("user_message_id", id.clone())
            .with("user_message_uuid", id)
    }

    /// `mcp_status`.
    #[must_use]
    pub fn mcp_status() -> Self {
        Self::new(MCP_STATUS)
    }

    /// One field of the request.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// One string field of the request.
    #[must_use]
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.field(key).and_then(Value::as_str)
    }
}

/// A `control_request` line, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequestEnvelope {
    /// Always `control_request`.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Correlates the answer with this request.
    pub request_id: String,
    /// The request itself.
    pub request: ControlRequestBody,
}

impl ControlRequestEnvelope {
    /// Wraps `request` under `request_id`.
    #[must_use]
    pub fn new(request_id: impl Into<String>, request: ControlRequestBody) -> Self {
        Self {
            message_type: "control_request".into(),
            request_id: request_id.into(),
            request,
        }
    }

    /// The line to write to the CLI's stdin, newline included.
    #[must_use]
    pub fn to_line(&self) -> String {
        to_line(self)
    }
}

/// The `response` object of a `control_response`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponseBody {
    /// `success` or `error` (kept as text so a new subtype still parses).
    pub subtype: String,
    /// The request this answers.
    pub request_id: String,
    /// The answer's data on success; absent for requests that return nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// The error message on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A machine-readable error code, when the CLI gives one (e.g. `invalid_mode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Any other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ControlResponseBody {
    /// A success answer to `request_id`.
    #[must_use]
    pub fn success(request_id: impl Into<String>, response: Option<Value>) -> Self {
        Self {
            subtype: "success".into(),
            request_id: request_id.into(),
            response,
            error: None,
            error_code: None,
            extra: Map::new(),
        }
    }

    /// An error answer to `request_id`.
    #[must_use]
    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            subtype: "error".into(),
            request_id: request_id.into(),
            response: None,
            error: Some(error.into()),
            error_code: None,
            extra: Map::new(),
        }
    }

    /// Whether the request succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.subtype == "success"
    }

    /// The requests still queued after an interrupt (`still_queued`), when the answer has them.
    #[must_use]
    pub fn still_queued(&self) -> Option<Vec<Value>> {
        self.response
            .as_ref()?
            .get("still_queued")?
            .as_array()
            .cloned()
    }
}

/// A `control_response` line, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponseEnvelope {
    /// Always `control_response`.
    #[serde(rename = "type")]
    pub message_type: String,
    /// The answer.
    pub response: ControlResponseBody,
}

impl ControlResponseEnvelope {
    /// Wraps `response`.
    #[must_use]
    pub fn new(response: ControlResponseBody) -> Self {
        Self {
            message_type: "control_response".into(),
            response,
        }
    }

    /// The line to write to the CLI's stdin, newline included.
    #[must_use]
    pub fn to_line(&self) -> String {
        to_line(self)
    }
}

fn to_line(value: &impl Serialize) -> String {
    // Serializing these maps of JSON values cannot fail.
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    line
}

/// The CLI asking whether a tool may run (`can_use_tool`, sent with
/// `--permission-prompt-tool stdio`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanUseToolRequest {
    /// The tool, e.g. `Bash` or `mcp__server__tool`.
    pub tool_name: String,
    /// The tool's input.
    #[serde(default)]
    pub input: Value,
    /// Permission updates the CLI suggests (e.g. "always allow").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Value>,
    /// The path that triggered the request, when a path rule did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    /// Why the CLI is asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<Value>,
    /// The tool call this request is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The subagent making the call, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Any other field.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A control request from the CLI, by kind.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum InboundControlRequest {
    /// `can_use_tool`.
    CanUseTool(CanUseToolRequest),
    /// `hook_callback`: a hook registered through `initialize`.
    HookCallback {
        /// The callback id given at registration.
        callback_id: String,
        /// The hook input.
        input: Value,
        /// The tool call the hook is about, if any.
        tool_use_id: Option<String>,
    },
    /// `mcp_message`: a JSON-RPC message for an SDK MCP server.
    McpMessage {
        /// The SDK server's name.
        server_name: String,
        /// The JSON-RPC message.
        message: Value,
    },
    /// Any other subtype (answered with an error by the client so the CLI never waits).
    Other(ControlRequestBody),
}

impl From<ControlRequestBody> for InboundControlRequest {
    fn from(body: ControlRequestBody) -> Self {
        let as_value = || Value::Object(body.fields.clone());
        match body.subtype.as_str() {
            CAN_USE_TOOL => match serde_json::from_value(as_value()) {
                Ok(request) => Self::CanUseTool(request),
                Err(_) => Self::Other(body),
            },
            HOOK_CALLBACK => Self::HookCallback {
                callback_id: body.str_field("callback_id").unwrap_or_default().to_owned(),
                input: body.field("input").cloned().unwrap_or(Value::Null),
                tool_use_id: body.str_field("tool_use_id").map(str::to_owned),
            },
            MCP_MESSAGE => Self::McpMessage {
                server_name: body.str_field("server_name").unwrap_or_default().to_owned(),
                message: body.field("message").cloned().unwrap_or(Value::Null),
            },
            _ => Self::Other(body),
        }
    }
}

/// The answer to a `can_use_tool` request, in the shape Claude Code expects.
///
/// An allow without `updated_input` echoes the original input, since the CLI requires one.
#[must_use]
pub fn permission_response_value(result: &PermissionResult, request: &CanUseToolRequest) -> Value {
    let mut answer = Map::new();
    match result {
        PermissionResult::Allow(allow) => {
            answer.insert("behavior".into(), "allow".into());
            answer.insert(
                "updatedInput".into(),
                allow
                    .updated_input
                    .clone()
                    .unwrap_or_else(|| request.input.clone()),
            );
            if let Some(updates) = &allow.updated_permissions {
                answer.insert(
                    "updatedPermissions".into(),
                    serde_json::to_value(updates).unwrap_or(Value::Null),
                );
            }
        }
        PermissionResult::Deny(deny) => {
            answer.insert("behavior".into(), "deny".into());
            answer.insert("message".into(), deny.message.clone().into());
            if deny.interrupt {
                answer.insert("interrupt".into(), true.into());
            }
        }
    }
    if let Some(id) = &request.tool_use_id {
        answer.insert("toolUseID".into(), id.clone().into());
    }
    Value::Object(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PermissionResultAllow, PermissionResultDeny};
    use serde_json::json;

    #[test]
    fn interrupt_request_line_matches_the_cli() {
        let line = ControlRequestEnvelope::new("r1", ControlRequestBody::interrupt()).to_line();
        assert!(line.ends_with('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({"type": "control_request", "request_id": "r1", "request": {"subtype": "interrupt"}})
        );
    }

    #[test]
    fn request_builders_carry_their_fields() {
        let cases = [
            (
                ControlRequestBody::set_model(Some("haiku".into())),
                json!({"subtype": "set_model", "model": "haiku"}),
            ),
            (
                ControlRequestBody::set_model(None),
                json!({"subtype": "set_model", "model": null}),
            ),
            (
                ControlRequestBody::set_permission_mode(PermissionMode::DontAsk),
                json!({"subtype": "set_permission_mode", "mode": "dontAsk"}),
            ),
            (
                ControlRequestBody::set_permission_mode(PermissionMode::Manual),
                json!({"subtype": "set_permission_mode", "mode": "default"}),
            ),
            (
                ControlRequestBody::set_max_thinking_tokens(Some(1000)),
                json!({"subtype": "set_max_thinking_tokens", "max_thinking_tokens": 1000}),
            ),
            (
                ControlRequestBody::rewind_files("u1"),
                json!({"subtype": "rewind_files", "user_message_id": "u1", "user_message_uuid": "u1"}),
            ),
            (
                ControlRequestBody::initialize(),
                json!({"subtype": "initialize"}),
            ),
            (
                ControlRequestBody::mcp_status(),
                json!({"subtype": "mcp_status"}),
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(serde_json::to_value(&body).unwrap(), expected);
        }
    }

    #[test]
    fn interrupt_response_from_cli_2_1_292_parses() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r2","response":{"still_queued":[]}}}"#;
        let envelope: ControlResponseEnvelope = serde_json::from_str(line).unwrap();
        assert!(envelope.response.is_success());
        assert_eq!(envelope.response.request_id, "r2");
        assert_eq!(envelope.response.still_queued(), Some(vec![]));
    }

    #[test]
    fn error_response_keeps_code_and_unknown_fields() {
        let line = r#"{"type":"control_response","response":{"subtype":"error","request_id":"r6","error":"Cannot set permission mode","error_code":"invalid_mode","pending_permission_requests":[],"novel":1}}"#;
        let envelope: ControlResponseEnvelope = serde_json::from_str(line).unwrap();
        let body = envelope.response;
        assert!(!body.is_success());
        assert_eq!(body.error.as_deref(), Some("Cannot set permission mode"));
        assert_eq!(body.error_code.as_deref(), Some("invalid_mode"));
        assert_eq!(body.extra.get("novel"), Some(&json!(1)));
        assert_eq!(body.still_queued(), None);
    }

    #[test]
    fn success_without_response_data_parses() {
        let line =
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r7"}}"#;
        let envelope: ControlResponseEnvelope = serde_json::from_str(line).unwrap();
        assert!(envelope.response.is_success());
        assert!(envelope.response.response.is_none());
    }

    #[test]
    fn inbound_requests_are_classified() {
        let can_use: ControlRequestEnvelope = serde_json::from_value(json!({
            "type": "control_request", "request_id": "c1",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"},
                        "tool_use_id": "t1", "permission_suggestions": [], "future": true}
        }))
        .unwrap();
        match InboundControlRequest::from(can_use.request) {
            InboundControlRequest::CanUseTool(request) => {
                assert_eq!(request.tool_name, "Bash");
                assert_eq!(request.input, json!({"command": "ls"}));
                assert_eq!(request.tool_use_id.as_deref(), Some("t1"));
                assert_eq!(request.extra.get("future"), Some(&json!(true)));
            }
            other => panic!("expected can_use_tool, got {other:?}"),
        }

        let hook = ControlRequestBody::new(HOOK_CALLBACK)
            .with("callback_id", "hook_0")
            .with("input", json!({"a": 1}));
        assert_eq!(
            InboundControlRequest::from(hook),
            InboundControlRequest::HookCallback {
                callback_id: "hook_0".into(),
                input: json!({"a": 1}),
                tool_use_id: None
            }
        );

        let mcp = ControlRequestBody::new(MCP_MESSAGE)
            .with("server_name", "tools")
            .with("message", json!({"jsonrpc": "2.0"}));
        assert!(matches!(
            InboundControlRequest::from(mcp),
            InboundControlRequest::McpMessage { ref server_name, .. } if server_name == "tools"
        ));

        let unknown = ControlRequestBody::new("elicitation").with("x", 1);
        assert_eq!(
            InboundControlRequest::from(unknown.clone()),
            InboundControlRequest::Other(unknown)
        );
    }

    #[test]
    fn permission_answers_match_the_cli_shape() {
        let request = CanUseToolRequest {
            tool_name: "Bash".into(),
            input: json!({"command": "ls"}),
            permission_suggestions: None,
            blocked_path: None,
            decision_reason: None,
            tool_use_id: Some("t1".into()),
            agent_id: None,
            extra: Map::new(),
        };
        let allow = PermissionResult::Allow(PermissionResultAllow {
            updated_input: None,
            updated_permissions: None,
        });
        assert_eq!(
            permission_response_value(&allow, &request),
            json!({"behavior": "allow", "updatedInput": {"command": "ls"}, "toolUseID": "t1"})
        );
        let deny = PermissionResult::Deny(PermissionResultDeny {
            message: "no".into(),
            interrupt: true,
        });
        assert_eq!(
            permission_response_value(&deny, &request),
            json!({"behavior": "deny", "message": "no", "interrupt": true, "toolUseID": "t1"})
        );
    }
}
