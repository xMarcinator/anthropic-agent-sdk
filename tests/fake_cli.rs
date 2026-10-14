//! Integration tests against a fake Claude Code CLI.
//!
//! This test binary is its own fake CLI: started with `FAKE_CLAUDE_CLI=1` it reads stream-JSON
//! on stdin and answers the way Claude Code 2.1.292 does (init, replayed user messages,
//! `command_lifecycle`, `rate_limit_event`, results with usage, control requests and responses,
//! `can_use_tool`). Without that variable it runs the tests below, each of which starts
//! `ClaudeSDKClient` (or `query`) on this executable. No real CLI or model is involved.
//!
//! The target sets `harness = false`, so `main` runs the tests itself; a free argument filters
//! them by name.

use anthropic_agent_sdk::control::wire::ControlRequestBody;
use anthropic_agent_sdk::types::{
    ClaudeAgentOptions, ClaudeAuth, ContentBlock, Effort, Message, PermissionMode,
    PermissionResult, PermissionResultAllow, PermissionResultDeny, RateLimitType,
};
use anthropic_agent_sdk::{
    ClaudeSDKClient, PermissionManager, ProcessSpawner, SpawnRequest, StreamExt, query,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const FAKE_ENV: &str = "FAKE_CLAUDE_CLI";
const WAIT: Duration = Duration::from_secs(20);

// ============================================================================
// The fake CLI
// ============================================================================

mod fake {
    use super::*;

    struct Cli {
        args: Vec<String>,
        out: std::io::Stdout,
        init_sent: bool,
        busy: Option<String>,
        awaiting_permission: Option<String>,
        model: Option<String>,
        mode: String,
        answered: u32,
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mode = arg_after(&args, "--permission-mode")
            .unwrap_or("default")
            .to_string();
        let model = arg_after(&args, "--model").map(str::to_string);
        let mut cli = Cli {
            args,
            out: std::io::stdout(),
            init_sent: false,
            busy: None,
            awaiting_permission: None,
            model,
            mode,
            answered: 0,
        };
        // String mode: the prompt follows `--` and stdin is closed.
        if let Some(prompt) = arg_after(&cli.args, "--").map(str::to_string) {
            cli.user("prompt-1", &prompt);
            return;
        }
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match value["type"].as_str() {
                Some("user") => {
                    let uuid = value["uuid"]
                        .as_str()
                        .unwrap_or("generated-uuid")
                        .to_string();
                    let text = match &value["message"]["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(blocks) => blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join(""),
                        _ => String::new(),
                    };
                    cli.user(&uuid, &text);
                }
                Some("control_request") => cli.control(&value),
                Some("control_response") => cli.permission_answer(&value),
                _ => {}
            }
        }
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    impl Cli {
        fn emit(&mut self, value: Value) {
            writeln!(self.out, "{value}").unwrap();
            self.out.flush().unwrap();
        }

        fn lifecycle(&mut self, uuid: &str, state: &str) {
            self.emit(
                json!({"type": "command_lifecycle", "command_uuid": uuid, "state": state,
                "uuid": format!("lc-{state}"), "session_id": "fake-session"}),
            );
        }

        fn init(&mut self) {
            if self.init_sent {
                return;
            }
            self.init_sent = true;
            let var = |name: &str| std::env::var(name).ok();
            self.emit(json!({
                "type": "system", "subtype": "init", "cwd": ".", "session_id": "fake-session",
                "tools": ["Bash", "Read"], "mcp_servers": [],
                "model": self.model.clone().unwrap_or_else(|| "claude-haiku-4-5-20251001".into()),
                "permissionMode": self.mode, "apiKeySource": "none",
                "claude_code_version": "2.1.292",
                "capabilities": ["interrupt_receipt_v1", "msg_lifecycle_v1"],
                "uuid": "init-uuid",
                "fake_args": self.args,
                "fake_env": {
                    "CLAUDE_CONFIG_DIR": var("CLAUDE_CONFIG_DIR"),
                    "has_oauth_token": var("CLAUDE_CODE_OAUTH_TOKEN").is_some(),
                    "has_api_key": var("ANTHROPIC_API_KEY").is_some(),
                    "entrypoint": var("CLAUDE_CODE_ENTRYPOINT"),
                }
            }));
        }

        fn user(&mut self, uuid: &str, text: &str) {
            self.lifecycle(uuid, "queued");
            self.lifecycle(uuid, "started");
            self.init();
            if self.args.iter().any(|a| a == "--replay-user-messages") {
                self.emit(
                    json!({"type": "user", "message": {"role": "user", "content": text},
                    "session_id": "fake-session", "parent_tool_use_id": null, "uuid": uuid,
                    "timestamp": "2026-10-14T09:09:52.289Z", "isReplay": true}),
                );
            }
            if text == "slow" {
                self.busy = Some(uuid.to_string());
                self.assistant("working...");
                return;
            }
            if text == "not json" {
                // A line that is not JSON, then the turn as usual.
                writeln!(self.out, "not json").unwrap();
                self.assistant("after the bad line");
                self.result(uuid, false, "success", None);
                return;
            }
            if text == "ask twice" {
                self.awaiting_permission = Some(uuid.to_string());
                for id in ["perm-a", "perm-b"] {
                    self.emit(
                        json!({"type": "control_request", "request_id": id, "request": {
                        "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": id},
                        "tool_use_id": format!("toolu_{id}"), "permission_suggestions": []}}),
                    );
                }
                return;
            }
            if text == "ask then withdraw" {
                self.emit(
                    json!({"type": "control_request", "request_id": "perm-w", "request": {
                    "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm"},
                    "tool_use_id": "toolu_w", "permission_suggestions": []}}),
                );
                std::thread::sleep(Duration::from_millis(200));
                self.emit(json!({"type": "control_cancel_request", "request_id": "perm-w"}));
                self.assistant("withdrawn");
                self.result(uuid, false, "success", None);
                return;
            }
            if let Some(tool) = text.strip_prefix("ask:") {
                self.awaiting_permission = Some(uuid.to_string());
                self.emit(json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "fake-session",
                    "message": {"model": "claude-haiku-4-5-20251001", "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": tool, "input": {"command": "ls"}}]}}));
                self.emit(
                    json!({"type": "control_request", "request_id": "perm-1", "request": {
                    "subtype": "can_use_tool", "tool_name": tool, "input": {"command": "ls"},
                    "tool_use_id": "toolu_1", "permission_suggestions": []}}),
                );
                return;
            }
            self.emit(json!({"type": "rate_limit_event", "rate_limit_info": {
                "status": "allowed_warning", "resetsAt": 1_760_450_400, "rateLimitType": "five_hour",
                "utilization": 0.82, "unifiedWindows": {
                    "five_hour": {"utilization": 0.82, "resetsAt": 1_760_450_400},
                    "seven_day": {"utilization": 0.41, "resetsAt": 1_760_900_000}}},
                "uuid": "rl-1", "session_id": "fake-session"}));
            self.emit(json!({"type": "future_event", "detail": 1}));
            self.emit(json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "fake-session",
                "uuid": "a-1", "message": {"model": "claude-haiku-4-5-20251001", "id": "msg_1",
                "stop_reason": "end_turn", "content": [
                    {"type": "redacted_thinking", "data": "xyz"},
                    {"type": "text", "text": format!("echo: {text}")}]}}));
            self.result(uuid, false, "success", None);
        }

        fn assistant(&mut self, text: &str) {
            self.emit(json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "fake-session",
                "message": {"model": "claude-haiku-4-5-20251001", "content": [{"type": "text", "text": text}]}}));
        }

        fn result(&mut self, uuid: &str, is_error: bool, subtype: &str, terminal: Option<&str>) {
            self.emit(
                json!({"type": "result", "subtype": subtype, "is_error": is_error,
                "duration_ms": 12, "duration_api_ms": 10, "num_turns": 1,
                "session_id": "fake-session", "total_cost_usd": 0.0123,
                "usage": {"input_tokens": 5, "output_tokens": 7, "cache_creation_input_tokens": 1,
                          "cache_read_input_tokens": 2, "service_tier": "standard"},
                "modelUsage": {"claude-haiku-4-5-20251001": {"inputTokens": 5, "outputTokens": 7,
                    "cacheCreationInputTokens": 1, "cacheReadInputTokens": 2, "costUSD": 0.0123,
                    "contextWindow": 200_000}},
                "terminal_reason": terminal.unwrap_or("completed"),
                "uuid": "res-1", "user_message_uuids": [uuid], "fast_mode_state": "off",
                "result": if is_error { "" } else { "done" }}),
            );
        }

        fn respond(&mut self, request_id: &str, response: Option<Value>) {
            let mut body = json!({"subtype": "success", "request_id": request_id});
            if let Some(response) = response {
                body["response"] = response;
            }
            self.emit(json!({"type": "control_response", "response": body}));
        }

        fn refuse(&mut self, request_id: &str, error: &str, code: Option<&str>) {
            let mut body = json!({"subtype": "error", "request_id": request_id, "error": error});
            if let Some(code) = code {
                body["error_code"] = json!(code);
            }
            self.emit(json!({"type": "control_response", "response": body}));
        }

        fn control(&mut self, value: &Value) {
            let id = value["request_id"].as_str().unwrap_or("").to_string();
            let request = &value["request"];
            match request["subtype"].as_str().unwrap_or("") {
                "interrupt" => {
                    self.respond(&id, Some(json!({"still_queued": []})));
                    if let Some(uuid) = self.busy.take() {
                        self.lifecycle(&uuid, "cancelled");
                        self.result(&uuid, true, "error_during_execution", Some("aborted"));
                    }
                }
                "initialize" => {
                    let pid = std::process::id();
                    self.respond(
                        &id,
                        Some(json!({"commands": [], "models": [{"value": "haiku",
                        "resolvedModel": "claude-haiku-4-5-20251001",
                        "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"]}],
                        "account": {"tokenSource": "none"}, "pid": pid,
                        "current_permission_mode": self.mode})),
                    );
                }
                "set_model" => {
                    self.model = request["model"].as_str().map(str::to_string);
                    self.respond(&id, None);
                }
                "set_permission_mode" => {
                    let mode = request["mode"].as_str().unwrap_or("").to_string();
                    if [
                        "acceptEdits",
                        "auto",
                        "bypassPermissions",
                        "default",
                        "dontAsk",
                        "plan",
                    ]
                    .contains(&mode.as_str())
                    {
                        self.mode = mode.clone();
                        self.respond(&id, Some(json!({"mode": mode})));
                    } else {
                        self.refuse(&id, "Cannot set permission mode", Some("invalid_mode"));
                    }
                }
                other => {
                    let message = format!("Unsupported control request subtype: {other}");
                    self.refuse(&id, &message, None);
                }
            }
        }

        fn permission_answer(&mut self, value: &Value) {
            let response = &value["response"];
            if response["request_id"] == "perm-w" {
                // The withdrawn request must never be answered.
                self.assistant("answered a withdrawn request");
                return;
            }
            if response["request_id"] == "perm-a" || response["request_id"] == "perm-b" {
                self.answered += 1;
                if self.answered == 2 {
                    if let Some(uuid) = self.awaiting_permission.take() {
                        self.assistant("both answered");
                        self.result(&uuid, false, "success", None);
                    }
                }
                return;
            }
            if response["request_id"] != "perm-1" {
                return;
            }
            let Some(uuid) = self.awaiting_permission.take() else {
                return;
            };
            let answer = &response["response"];
            let behavior = answer["behavior"].as_str().unwrap_or("none").to_string();
            let echoed = answer["toolUseID"].as_str().unwrap_or("").to_string();
            self.assistant(&format!("permission: {behavior} for {echoed}"));
            self.result(&uuid, false, "success", None);
        }
    }
}

// ============================================================================
// Test harness
// ============================================================================

type TestFn = fn() -> Pin<Box<dyn Future<Output = ()> + Send>>;

fn main() {
    if std::env::var(FAKE_ENV).as_deref() == Ok("1") {
        fake::run();
        return;
    }
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let tests: Vec<(&str, TestFn)> = vec![
        ("replay_round_trip", || Box::pin(replay_round_trip())),
        ("interrupt_and_wait_round_trip", || {
            Box::pin(interrupt_and_wait_round_trip())
        }),
        ("fire_and_forget_interrupt", || {
            Box::pin(fire_and_forget_interrupt())
        }),
        ("control_requests_round_trip", || {
            Box::pin(control_requests_round_trip())
        }),
        ("can_use_tool_round_trip", || {
            Box::pin(can_use_tool_round_trip())
        }),
        ("manual_permission_round_trip", || {
            Box::pin(manual_permission_round_trip())
        }),
        ("query_string_mode", || Box::pin(query_string_mode())),
        ("multiple_successive_messages", || {
            Box::pin(multiple_successive_messages())
        }),
        ("client_lifecycle_and_receivers", || {
            Box::pin(client_lifecycle_and_receivers())
        }),
        ("client_creation_with_invalid_cli", || {
            Box::pin(client_creation_with_invalid_cli())
        }),
        ("spawner_stdout_and_exit", || {
            Box::pin(spawner_stdout_and_exit())
        }),
        ("a_line_that_is_not_json_is_skipped", || {
            Box::pin(a_line_that_is_not_json_is_skipped())
        }),
        ("permission_requests_are_answered_concurrently", || {
            Box::pin(permission_requests_are_answered_concurrently())
        }),
        ("a_withdrawn_permission_request_drops_its_callback", || {
            Box::pin(a_withdrawn_permission_request_drops_its_callback())
        }),
        ("a_killed_process_reports_its_exit", || {
            Box::pin(a_killed_process_reports_its_exit())
        }),
    ];
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, test) in tests {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        ran += 1;
        let outcome = runtime.block_on(async {
            match timeout(Duration::from_secs(60), tokio::spawn(test())).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("panicked: {e}")),
                Err(_) => Err("timed out".to_string()),
            }
        });
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                println!("test {name} ... FAILED ({e})");
                failed.push(name);
            }
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        ran - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(1);
    }
}

/// Options that start this executable as the fake CLI.
fn fake_options() -> ClaudeAgentOptions {
    let mut options = ClaudeAgentOptions::builder()
        .path_to_claude_code_executable(std::env::current_exe().expect("test executable"))
        .build();
    options.env.insert(FAKE_ENV.into(), "1".into());
    options
}

async fn client(options: ClaudeAgentOptions) -> ClaudeSDKClient {
    ClaudeSDKClient::new(options, None)
        .await
        .expect("fake CLI starts")
}

/// Messages up to and including the next result.
async fn until_result(client: &mut ClaudeSDKClient) -> Vec<Message> {
    let mut messages = Vec::new();
    loop {
        let message = timeout(WAIT, client.next_message())
            .await
            .expect("a message in time")
            .expect("stream open")
            .expect("message parses");
        let done = matches!(message, Message::Result { .. });
        messages.push(message);
        if done {
            return messages;
        }
    }
}

fn init_of(messages: &[Message]) -> anthropic_agent_sdk::types::SystemInit {
    messages
        .iter()
        .find_map(Message::as_system_init)
        .expect("init message")
}

// ============================================================================
// Tests
// ============================================================================

async fn replay_round_trip() {
    let mut options = fake_options();
    options.model = Some("opus".into());
    options.model_context = Some("1m".into());
    options.effort = Some(Effort::High);
    options.auth = ClaudeAuth::OAuthToken("setup-token-value".into());
    options.config_dir = Some(PathBuf::from("fake-config-dir"));
    let mut client = client(options).await;
    client
        .send_message_with_uuid("hello", "11111111-1111-4111-8111-111111111111")
        .await
        .unwrap();
    let messages = until_result(&mut client).await;

    let uuid = "11111111-1111-4111-8111-111111111111";
    assert!(messages.iter().any(|m| matches!(m,
        Message::CommandLifecycle { command_uuid, state, .. } if command_uuid == uuid && state == "queued")));
    let echo = messages
        .iter()
        .find(|m| m.is_replay())
        .expect("replayed user message");
    assert!(matches!(echo, Message::User { uuid: Some(u), .. } if u == uuid));

    let init = init_of(&messages);
    assert_eq!(init.claude_code_version.as_deref(), Some("2.1.292"));
    assert_eq!(init.model.as_deref(), Some("opus[1m]"));
    let args: Vec<String> = serde_json::from_value(init.extra["fake_args"].clone()).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--effort" && w[1] == "high")
    );
    let env = &init.extra["fake_env"];
    assert_eq!(env["CLAUDE_CONFIG_DIR"], "fake-config-dir");
    assert_eq!(env["has_oauth_token"], true);
    assert_eq!(env["has_api_key"], false);
    assert_eq!(env["entrypoint"], "sdk-rust");

    let info = messages
        .iter()
        .find_map(|m| match m {
            Message::RateLimitEvent {
                rate_limit_info, ..
            } => Some(rate_limit_info.clone()),
            _ => None,
        })
        .expect("rate limit event");
    assert_eq!(
        info.window(&RateLimitType::SevenDay).unwrap().utilization,
        Some(0.41)
    );
    assert!(messages.iter().any(|m| m.message_type() == "future_event"));
    let blocks = messages
        .iter()
        .find_map(|m| match m {
            Message::Assistant { message, .. } => Some(message.content.clone()),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&blocks[0], ContentBlock::Other(_)));
    assert!(matches!(&blocks[1], ContentBlock::Text { text } if text == "echo: hello"));

    let result = messages.last().unwrap();
    assert_eq!(result.result_usage().unwrap().output_tokens, 7);
    match result {
        Message::Result {
            total_cost_usd,
            model_usage,
            user_message_uuids,
            ..
        } => {
            assert_eq!(*total_cost_usd, Some(0.0123));
            assert_eq!(
                model_usage["claude-haiku-4-5-20251001"].context_window,
                200_000
            );
            assert_eq!(user_message_uuids, &vec![uuid.to_string()]);
        }
        _ => unreachable!(),
    }
    assert_eq!(client.get_session_id().as_deref(), Some("fake-session"));
    client.close().await.unwrap();
}

async fn interrupt_and_wait_round_trip() {
    let mut client = client(fake_options()).await;
    client
        .send_message_with_uuid("slow", "u-slow")
        .await
        .unwrap();
    // Wait until the turn is running.
    loop {
        let message = timeout(WAIT, client.next_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(message, Message::Assistant { .. }) {
            break;
        }
    }
    let answer = client
        .interrupt_and_wait(WAIT)
        .await
        .expect("interrupt answered");
    assert!(answer.is_success());
    assert_eq!(answer.still_queued(), Some(vec![]));
    let messages = until_result(&mut client).await;
    // The control response itself never reaches the message stream.
    assert!(
        !messages
            .iter()
            .any(|m| m.message_type() == "control_response")
    );
    assert!(messages.iter().any(|m| matches!(m,
        Message::CommandLifecycle { state, command_uuid, .. } if state == "cancelled" && command_uuid == "u-slow")));
    assert!(
        matches!(messages.last(), Some(Message::Result { is_error: true, terminal_reason: Some(r), .. }) if r == "aborted")
    );
    client.close().await.unwrap();
}

async fn fire_and_forget_interrupt() {
    let mut client = client(fake_options()).await;
    client.send_message("slow").await.unwrap();
    client.interrupt().await.unwrap();
    let messages = until_result(&mut client).await;
    assert!(matches!(
        messages.last(),
        Some(Message::Result { is_error: true, .. })
    ));
    client.close().await.unwrap();
}

async fn control_requests_round_trip() {
    let mut client = client(fake_options()).await;
    let info = client.initialize().await.expect("initialize");
    assert_eq!(info["models"][0]["supportedEffortLevels"][3], "xhigh");
    client.change_model(Some("sonnet".into())).await.unwrap();
    assert_eq!(client.get_runtime_model().as_deref(), Some("sonnet"));
    client
        .change_permission_mode(PermissionMode::Manual)
        .await
        .unwrap();
    client
        .change_permission_mode(PermissionMode::Auto)
        .await
        .unwrap();
    assert_eq!(
        client.get_runtime_permission_mode(),
        Some(PermissionMode::Auto)
    );
    let refused = client
        .send_control_request(
            ControlRequestBody::new("set_permission_mode").with("mode", "manual"),
            WAIT,
        )
        .await
        .expect_err("manual is refused on the control channel");
    assert!(refused.to_string().contains("Cannot set permission mode"));
    let unknown = client
        .send_control_request(ControlRequestBody::new("no_such_thing"), WAIT)
        .await
        .expect_err("unknown subtype refused");
    assert!(
        unknown
            .to_string()
            .contains("Unsupported control request subtype")
    );
    // The session still works after control traffic.
    client.send_message("after").await.unwrap();
    let messages = until_result(&mut client).await;
    assert_eq!(init_of(&messages).model.as_deref(), Some("sonnet"));
    client.close().await.unwrap();
}

async fn can_use_tool_round_trip() {
    let mut options = fake_options();
    options.can_use_tool = Some(PermissionManager::callback(
        |tool, input, _ctx| async move {
            assert_eq!(input, json!({"command": "ls"}));
            Ok(if tool == "Bash" {
                PermissionResult::Deny(PermissionResultDeny {
                    message: "no shell".into(),
                    interrupt: false,
                })
            } else {
                PermissionResult::Allow(PermissionResultAllow {
                    updated_input: None,
                    updated_permissions: None,
                })
            })
        },
    ));
    let mut client = client(options).await;
    client.send_message("ask:Bash").await.unwrap();
    let messages = until_result(&mut client).await;
    let init = init_of(&messages);
    let args: Vec<String> = serde_json::from_value(init.extra["fake_args"].clone()).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--permission-prompt-tool" && w[1] == "stdio")
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.message_type() == "control_request")
    );
    let said: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { message, .. } => message.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert!(
        said.contains(&"permission: deny for toolu_1".to_string()),
        "{said:?}"
    );

    client.send_message("ask:Read").await.unwrap();
    let messages = until_result(&mut client).await;
    assert!(messages.iter().any(|m| matches!(m, Message::Assistant { message, .. }
        if matches!(message.content.first(), Some(ContentBlock::Text { text }) if text == "permission: allow for toolu_1"))));
    client.close().await.unwrap();
}

async fn manual_permission_round_trip() {
    // Without a callback, `stdio` permission prompts go to the permission receiver.
    let mut options = fake_options();
    options.permission_prompt_tool_name = Some("stdio".into());
    let mut client = client(options).await;
    let mut requests = client.take_permission_receiver().expect("receiver");
    client.send_message("ask:Edit").await.unwrap();
    let (request_id, request) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert_eq!(request.tool_name.as_str(), "Edit");
    client
        .respond_to_permission(
            request_id,
            PermissionResult::Allow(PermissionResultAllow {
                updated_input: Some(json!({"command": "ls -la"})),
                updated_permissions: None,
            }),
        )
        .await
        .unwrap();
    let messages = until_result(&mut client).await;
    assert!(messages.iter().any(|m| matches!(m, Message::Assistant { message, .. }
        if matches!(message.content.first(), Some(ContentBlock::Text { text }) if text == "permission: allow for toolu_1"))));
    client.close().await.unwrap();
}

async fn query_string_mode() {
    let stream = query("one-shot", Some(fake_options())).await.unwrap();
    let messages: Vec<Message> = Box::pin(stream).map(|m| m.expect("parses")).collect().await;
    assert!(messages.iter().any(|m| m.as_system_init().is_some()));
    assert!(matches!(
        messages.last(),
        Some(Message::Result {
            is_error: false,
            ..
        })
    ));
    let init = init_of(&messages);
    let args: Vec<String> = serde_json::from_value(init.extra["fake_args"].clone()).unwrap();
    assert!(!args.contains(&"--replay-user-messages".to_string()));
}

async fn multiple_successive_messages() {
    let mut client = client(fake_options()).await;
    for i in 0..3 {
        client.send_message(format!("Message {i}")).await.unwrap();
    }
    let mut texts = HashMap::new();
    for _ in 0..3 {
        for message in until_result(&mut client).await {
            if let Message::Assistant { message, .. } = message {
                for block in message.content {
                    if let ContentBlock::Text { text } = block {
                        texts.insert(text, ());
                    }
                }
            }
        }
    }
    for i in 0..3 {
        assert!(texts.contains_key(&format!("echo: Message {i}")));
    }
    client.close().await.unwrap();
}

async fn client_lifecycle_and_receivers() {
    let mut client = client(fake_options()).await;
    assert!(client.is_connected().await);
    assert!(client.get_session_id().is_none());
    assert!(client.take_hook_receiver().is_some());
    assert!(client.take_hook_receiver().is_none());
    assert!(client.take_permission_receiver().is_some());
    assert!(client.take_permission_receiver().is_none());
    client.close().await.unwrap();
}

async fn client_creation_with_invalid_cli() {
    let result = ClaudeSDKClient::new(
        ClaudeAgentOptions::default(),
        Some(PathBuf::from("/nonexistent/claude")),
    )
    .await;
    assert!(result.is_err());
}

/// Starts the request as it is and counts the starts.
struct CountingSpawner(std::sync::atomic::AtomicUsize);

impl ProcessSpawner for CountingSpawner {
    fn spawn(&self, request: SpawnRequest) -> std::io::Result<tokio::process::Child> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        request.command().spawn()
    }
}

async fn spawner_stdout_and_exit() {
    let spawner = Arc::new(CountingSpawner(Default::default()));
    let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut options = fake_options();
    options.spawner = Some(spawner.clone());
    let seen = lines.clone();
    options.stdout = Some(Arc::new(move |line| seen.lock().unwrap().push(line)));
    let mut client = client(options).await;
    assert_eq!(spawner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let process = client.process().expect("a process");
    assert!(process.id().is_some());
    assert!(process.exited().is_none());
    client.send_message("hello").await.unwrap();
    until_result(&mut client).await;
    // Closing stdin ends the fake CLI, which exits with 0.
    client.end_input().await.unwrap();
    let exit = timeout(WAIT, process.wait()).await.expect("exits");
    assert_eq!(exit.code, Some(0));
    assert_eq!(process.exited(), Some(exit));
    let lines = lines.lock().unwrap();
    assert!(lines.iter().any(|line| line.contains(r#""type":"result""#)));
    assert!(lines.iter().all(|line| !line.ends_with('\n')));
}

async fn a_line_that_is_not_json_is_skipped() {
    let mut client = client(fake_options()).await;
    client.send_message("not json").await.unwrap();
    let mut errors = 0;
    let mut texts = Vec::new();
    loop {
        let message = timeout(WAIT, client.next_message())
            .await
            .expect("a message in time")
            .expect("stream open");
        match message {
            Err(_) => errors += 1,
            Ok(Message::Assistant { message, .. }) => {
                texts.extend(message.content.into_iter().filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text),
                    _ => None,
                }));
            }
            Ok(Message::Result { .. }) => break,
            Ok(_) => {}
        }
    }
    assert_eq!(errors, 1);
    assert_eq!(texts, ["after the bad line"]);
    client.close().await.unwrap();
}

async fn permission_requests_are_answered_concurrently() {
    // The first answer waits for the second request to arrive: answered one at a time, the
    // two would never finish.
    let second = Arc::new(tokio::sync::Notify::new());
    let mut options = fake_options();
    let arrived = second.clone();
    options.can_use_tool = Some(PermissionManager::callback(move |_tool, input, _ctx| {
        let arrived = arrived.clone();
        async move {
            if input["command"] == "perm-a" {
                arrived.notified().await;
            } else {
                arrived.notify_one();
            }
            Ok(PermissionResult::Allow(PermissionResultAllow {
                updated_input: None,
                updated_permissions: None,
            }))
        }
    }));
    let mut client = client(options).await;
    client.send_message("ask twice").await.unwrap();
    let messages = until_result(&mut client).await;
    assert!(messages.iter().any(|m| matches!(m, Message::Assistant { message, .. }
        if matches!(message.content.first(), Some(ContentBlock::Text { text }) if text == "both answered"))));
    client.close().await.unwrap();
}

/// Reports when the permission callback's future is dropped.
struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for Dropped {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn a_withdrawn_permission_request_drops_its_callback() {
    let (sender, dropped) = tokio::sync::oneshot::channel();
    let guard = Arc::new(std::sync::Mutex::new(Some(Dropped(Some(sender)))));
    let mut options = fake_options();
    options.can_use_tool = Some(PermissionManager::callback(move |_tool, _input, _ctx| {
        let guard = guard.lock().unwrap().take();
        async move {
            let _guard = guard;
            // Never answers on its own.
            std::future::pending::<()>().await;
            unreachable!()
        }
    }));
    let mut client = client(options).await;
    client.send_message("ask then withdraw").await.unwrap();
    let messages = until_result(&mut client).await;
    timeout(WAIT, dropped)
        .await
        .expect("the callback is dropped")
        .expect("dropped once");
    assert!(!messages.iter().any(|m| matches!(m, Message::Assistant { message, .. }
        if matches!(message.content.first(), Some(ContentBlock::Text { text }) if text.contains("withdrawn request")))));
    client.close().await.unwrap();
}

async fn a_killed_process_reports_its_exit() {
    let mut client = client(fake_options()).await;
    client.send_message("slow").await.unwrap();
    let process = client.process().expect("a process");
    process.kill();
    let exit = timeout(WAIT, process.wait()).await.expect("exits");
    assert_ne!(exit.code, Some(0));
    // The stream ends once the process is gone.
    while let Some(_message) = timeout(WAIT, client.next_message()).await.expect("ends") {}
}
