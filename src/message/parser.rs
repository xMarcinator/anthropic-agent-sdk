//! Message parser for Claude Code SDK responses

use crate::error::{ClaudeError, Result};
use crate::types::Message;

/// Parse a JSON value into a typed Message
///
/// # Arguments
/// * `data` - Raw JSON value from CLI output
///
/// # Returns
/// Parsed Message object or error
///
/// # Errors
/// Returns `ClaudeError::MessageParse` only when the value is not a JSON object with a string
/// `type`. Unknown message types, and known types whose payload no longer matches, parse as
/// [`Message::Other`] so newer Claude Code versions never break the stream.
///
/// # Security Note
/// For additional security against deeply nested JSON attacks, consider using
/// `serde_json::Deserializer::with_recursion_limit()` to set explicit depth limits.
/// This is not currently implemented to avoid additional complexity, but the current
/// timeout mechanism on read operations provides some protection against excessive
/// parsing time.
pub fn parse_message(data: serde_json::Value) -> Result<Message> {
    // Debug log for result messages with structured_output
    if data.get("type") == Some(&serde_json::json!("result")) {
        tracing::debug!(
            "Parsing result message, structured_output in JSON: {:?}",
            data.get("structured_output")
        );
    }

    if data
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return Err(ClaudeError::message_parse(
            "Failed to parse message: not a JSON object with a string `type`",
            Some(data),
        ));
    }

    serde_json::from_value(data.clone()).map_err(|e| {
        ClaudeError::message_parse(format!("Failed to parse message: {e}"), Some(data))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, RateLimitType, UserContent};
    use serde_json::json;

    fn parse(line: &str) -> Message {
        parse_message(serde_json::from_str(line).expect("valid JSON")).expect("parses")
    }

    #[test]
    fn test_parse_user_message() {
        let data = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "Hello, Claude!"
            }
        });

        let result = parse_message(data);
        assert!(result.is_ok());
    }

    #[test]
    fn unknown_message_types_are_kept_not_rejected() {
        let data = json!({"type": "invalid_type", "data": "some data"});
        match parse_message(data.clone()).unwrap() {
            Message::Other(value) => assert_eq!(value, data),
            other => panic!("expected Other, got {other:?}"),
        }
        let message =
            parse(r#"{"type":"tool_progress","tool_use_id":"t1","elapsed_time_seconds":3}"#);
        assert_eq!(message.message_type(), "tool_progress");
    }

    #[test]
    fn a_non_object_still_fails() {
        assert!(parse_message(json!("text")).is_err());
        assert!(parse_message(json!(42)).is_err());
    }

    /// The init message of Claude Code 2.1.292, lists shortened.
    const INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/work","session_id":"957db446-e0b2-403d-96d3-e900443a4648","tools":["Task","Bash","mcp__tracker__permission_prompt"],"mcp_servers":[{"name":"tracker","status":"connected"}],"model":"claude-haiku-4-5-20251001","permissionMode":"default","slash_commands":["compact","context"],"terminal_slash_commands":["doctor"],"apiKeySource":"none","claude_code_version":"2.1.292","output_style":"default","agents":["claude","Explore"],"skills":["dataviz"],"plugins":[{"name":"p","path":"builtin","source":"p@builtin"}],"capabilities":["interrupt_receipt_v1","interrupt_cancel_queued_v1","msg_lifecycle_v1"],"analytics_disabled":false,"uuid":"af124d44-b753-4cfd-b8d2-4585a0e329fe","fast_mode_state":"off","per_turn_effort_active":false}"#;

    #[test]
    fn system_init_of_2_1_292_parses_typed() {
        let message = parse(INIT);
        let init = message.as_system_init().expect("init");
        assert_eq!(
            init.session_id.as_deref(),
            Some("957db446-e0b2-403d-96d3-e900443a4648")
        );
        assert_eq!(init.model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(init.permission_mode.as_deref(), Some("default"));
        assert_eq!(init.claude_code_version.as_deref(), Some("2.1.292"));
        assert!(init.is_subscription());
        assert!(init.has_capability("interrupt_receipt_v1"));
        assert!(init.has_capability("msg_lifecycle_v1"));
        assert_eq!(init.tools.len(), 3);
        assert_eq!(init.skills, vec!["dataviz".to_string()]);
        assert_eq!(init.extra.get("fast_mode_state"), Some(&json!("off")));
        assert!(
            parse(
                r#"{"type":"system","subtype":"status","status":null,"permissionMode":"default"}"#
            )
            .as_system_init()
            .is_none()
        );
    }

    #[test]
    fn replayed_user_message_keeps_uuid_and_flags() {
        let message = parse(
            r#"{"type":"user","message":{"role":"user","content":"hi"},"session_id":"s1","parent_tool_use_id":null,"uuid":"11111111-1111-4111-8111-111111111111","timestamp":"2026-10-14T09:09:52.289Z","isReplay":true}"#,
        );
        assert!(message.is_replay());
        match message {
            Message::User {
                uuid,
                is_synthetic,
                timestamp,
                message,
                ..
            } => {
                assert_eq!(
                    uuid.as_deref(),
                    Some("11111111-1111-4111-8111-111111111111")
                );
                assert!(!is_synthetic);
                assert_eq!(timestamp.as_deref(), Some("2026-10-14T09:09:52.289Z"));
                assert!(matches!(message.content, Some(UserContent::String(ref s)) if s == "hi"));
            }
            other => panic!("expected user, got {other:?}"),
        }
        let synthetic = parse(
            r#"{"type":"user","isReplay":true,"isSynthetic":true,"uuid":"c1","message":{"role":"user","content":"x"}}"#,
        );
        assert!(matches!(
            synthetic,
            Message::User {
                is_synthetic: true,
                ..
            }
        ));
    }

    #[test]
    fn assistant_auth_error_of_2_1_292_parses() {
        let message = parse(
            r#"{"type":"assistant","message":{"diagnostics":null,"id":"55414ab4","container":null,"model":"<synthetic>","role":"assistant","stop_details":null,"stop_reason":"stop_sequence","stop_sequence":"","type":"message","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Not logged in · Please run /login"}],"context_management":null},"parent_tool_use_id":null,"session_id":"s1","uuid":"eabb790a","timestamp":"2026-10-14T09:09:52.478Z","error":"authentication_failed","is_api_error_message":true,"user_message_uuid":"u1"}"#,
        );
        match message {
            Message::Assistant {
                message,
                error,
                uuid,
                ..
            } => {
                assert_eq!(error.as_deref(), Some("authentication_failed"));
                assert_eq!(uuid.as_deref(), Some("eabb790a"));
                assert_eq!(message.model, "<synthetic>");
                assert_eq!(message.stop_reason.as_deref(), Some("stop_sequence"));
                assert!(
                    matches!(&message.content[0], ContentBlock::Text { text } if text.starts_with("Not logged in"))
                );
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn unknown_content_blocks_do_not_fail_the_message() {
        let message = parse(
            r#"{"type":"assistant","message":{"model":"m","content":[{"type":"redacted_thinking","data":"x"},{"type":"thinking","thinking":"hmm"},{"type":"server_tool_use","id":"s","name":"web_search","input":{}}]}}"#,
        );
        let Message::Assistant { message, .. } = message else {
            panic!("expected assistant");
        };
        assert!(
            matches!(&message.content[0], ContentBlock::Other(v) if v["type"] == "redacted_thinking")
        );
        assert!(
            matches!(&message.content[1], ContentBlock::Thinking { signature, .. } if signature.is_empty())
        );
        assert_eq!(message.content.len(), 3);
    }

    #[test]
    fn result_of_2_1_292_parses_with_usage_cost_and_extras() {
        let message = parse(
            r#"{"duration_api_ms":0,"stop_reason":"stop_sequence","session_id":"s1","total_cost_usd":1.234567,"usage":{"output_tokens_details":{"thinking_tokens":0},"input_tokens":5,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":2,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard"},"modelUsage":{"claude-opus-5-5":{"inputTokens":8,"outputTokens":18,"cacheCreationInputTokens":30,"cacheReadInputTokens":40,"costUSD":1.2,"webSearchRequests":0,"contextWindow":1000000,"maxOutputTokens":64000},"claude-haiku-4-5":{"inputTokens":2,"outputTokens":2,"costUSD":0.03}},"permission_denials":[],"terminal_reason":"api_error","fast_mode_state":"off","subagent_stats":{"spawned":0},"safety_stops":0,"is_error":true,"num_turns":1,"subtype":"success","api_error_status":null,"result":"Not logged in · Please run /login","type":"result","duration_ms":277,"uuid":"bbaba287","user_message_uuid":"u1","user_message_uuids":["u1"],"queued_turn_count":0,"result_index":0}"#,
        );
        let usage = message.result_usage().expect("usage");
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cache_creation_input_tokens, 3);
        assert_eq!(usage.cache_read_input_tokens, 4);
        assert_eq!(usage.extra.get("service_tier"), Some(&json!("standard")));
        match message {
            Message::Result {
                total_cost_usd,
                model_usage,
                is_error,
                terminal_reason,
                user_message_uuids,
                uuid,
                extra,
                ..
            } => {
                assert_eq!(total_cost_usd, Some(1.234_567));
                assert!(is_error);
                assert_eq!(terminal_reason.as_deref(), Some("api_error"));
                assert_eq!(user_message_uuids, vec!["u1".to_string()]);
                assert_eq!(uuid.as_deref(), Some("bbaba287"));
                assert_eq!(model_usage["claude-opus-5-5"].output_tokens, 18);
                assert_eq!(model_usage["claude-opus-5-5"].context_window, 1_000_000);
                assert!((model_usage["claude-haiku-4-5"].cost_usd - 0.03).abs() < 1e-9);
                assert_eq!(extra.get("fast_mode_state"), Some(&json!("off")));
                assert_eq!(extra.get("queued_turn_count"), Some(&json!(0)));
                assert!(!extra.contains_key("type"));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    #[test]
    fn minimal_result_parses_with_defaults() {
        let message = parse(r#"{"type":"result","subtype":"success"}"#);
        assert!(matches!(
            message,
            Message::Result {
                num_turns: 0,
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn rate_limit_event_with_windows_parses() {
        let message = parse(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1760450400,"rateLimitType":"five_hour","utilization":0.82,"unifiedWindows":{"five_hour":{"utilization":0.82,"resetsAt":1760450400},"seven_day":{"utilization":0.41,"resetsAt":1760900000},"seven_day_fable":{"utilization":0.1,"resetsAt":1760900001}},"overageStatus":"allowed","isUsingOverage":false,"surpassedThreshold":0.8},"uuid":"r1","session_id":"s1"}"#,
        );
        let Message::RateLimitEvent {
            rate_limit_info: info,
            uuid,
            session_id,
        } = message
        else {
            panic!("expected rate_limit_event");
        };
        assert_eq!(uuid.as_deref(), Some("r1"));
        assert_eq!(session_id.as_deref(), Some("s1"));
        assert_eq!(info.status, "allowed_warning");
        assert!(!info.is_rejected());
        assert_eq!(info.rate_limit_type, Some(RateLimitType::FiveHour));
        assert_eq!(info.overage_status.as_deref(), Some("allowed"));
        assert_eq!(info.extra.get("surpassedThreshold"), Some(&json!(0.8)));
        let five = info.window(&RateLimitType::FiveHour).unwrap();
        assert_eq!(five.resets_at, Some(1_760_450_400));
        let seven = info.window(&RateLimitType::SevenDay).unwrap();
        assert_eq!(seven.utilization, Some(0.41));
        // A model-scoped window this SDK does not name still arrives.
        let fable = info
            .window(&RateLimitType::from("seven_day_fable"))
            .unwrap();
        assert_eq!(fable.resets_at, Some(1_760_900_001));
        assert!(info.window(&RateLimitType::SevenDayOpus).is_none());
    }

    #[test]
    fn minimal_and_future_rate_limit_events_parse() {
        let message = parse(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day_newmodel","resetsAt":5}}"#,
        );
        let Message::RateLimitEvent {
            rate_limit_info, ..
        } = message
        else {
            panic!("expected rate_limit_event");
        };
        assert!(rate_limit_info.is_rejected());
        assert_eq!(
            rate_limit_info.rate_limit_type,
            Some(RateLimitType::Other("seven_day_newmodel".into()))
        );
        // The top-level fields stand for the named window when no unified windows came.
        let window = rate_limit_info
            .window(&RateLimitType::from("seven_day_newmodel"))
            .unwrap();
        assert_eq!(window.resets_at, Some(5));
    }

    #[test]
    fn command_lifecycle_parses() {
        let message = parse(
            r#"{"type":"command_lifecycle","command_uuid":"u1","state":"queued","uuid":"x","session_id":"s1"}"#,
        );
        assert!(matches!(
            message,
            Message::CommandLifecycle { ref command_uuid, ref state, .. } if command_uuid == "u1" && state == "queued"
        ));
    }

    #[test]
    fn control_messages_parse() {
        let response = parse(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r2","response":{"still_queued":[]}}}"#,
        );
        assert!(
            matches!(response, Message::ControlResponse { ref response } if response.request_id == "r2" && response.is_success())
        );
        let request = parse(
            r#"{"type":"control_request","request_id":"c1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{}}}"#,
        );
        assert!(
            matches!(request, Message::ControlRequest { ref request, .. } if request.subtype == "can_use_tool")
        );
        let cancel = parse(r#"{"type":"control_cancel_request","request_id":"c1"}"#);
        assert_eq!(cancel.message_type(), "control_cancel_request");
    }

    #[test]
    fn a_known_type_with_a_broken_payload_degrades_to_other() {
        let message = parse(r#"{"type":"assistant","message":"not an object"}"#);
        assert!(matches!(message, Message::Other(_)));
        assert_eq!(message.message_type(), "assistant");
    }
}
