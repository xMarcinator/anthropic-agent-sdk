//! Test example for `SubagentStart` and `SubagentStop` hooks
//!
//! This example demonstrates:
//! - Registering hooks for `SubagentStart` and `SubagentStop` events
//! - Tracking subagent lifecycle (similar to codanna's subagent-stop.js)
//! - Using `process_message` to invoke hooks from the message stream
//!
//! Run with: cargo run --example `hook_subagent_test`
//! Run with debug: `RUST_LOG=debug` cargo run --example `hook_subagent_test`
//! Run with trace: `RUST_LOG=trace` cargo run --example `hook_subagent_test`

use anthropic_agent_sdk::ClaudeSDKClient;
use anthropic_agent_sdk::hooks::{HookManager, HookMatcherBuilder};
use anthropic_agent_sdk::types::{
    ClaudeAgentOptions, ContentBlock, HookEvent, HookOutput, Message,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// State to track subagent lifecycle
#[derive(Debug, Default)]
struct SubagentTracker {
    started: Vec<SubagentInfo>,
    stopped: Vec<SubagentInfo>,
}

#[derive(Debug, Clone)]
struct SubagentInfo {
    agent_id: String,
    agent_type: String,
    timestamp: std::time::Instant,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing with colors and env filter
    // Use RUST_LOG env var to control level: error, warn, info, debug, trace
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_ansi(true) // Enable colors
        .init();

    info!("=== Subagent Hook Test ===");

    // Shared state to track subagents
    let tracker = Arc::new(Mutex::new(SubagentTracker::default()));

    // Create SubagentStart hook
    let tracker_start = tracker.clone();
    let subagent_start_hook = HookManager::callback(move |input, _tool_name, ctx| {
        let _ = ctx; // Future: abort signal support
        let tracker = tracker_start.clone();
        async move {
            let agent_id = input
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let agent_type = input
                .get("agent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            info!(
                agent_id = %agent_id,
                agent_type = %agent_type,
                "Subagent started"
            );

            // Use std::sync::Mutex - quick lock for Vec push
            {
                let mut t = tracker.lock().unwrap();
                t.started.push(SubagentInfo {
                    agent_id,
                    agent_type,
                    timestamp: std::time::Instant::now(),
                });
            }

            Ok(HookOutput::default())
        }
    });

    // Create SubagentStop hook
    let tracker_stop = tracker.clone();
    let subagent_stop_hook = HookManager::callback(move |input, _tool_name, ctx| {
        let _ = ctx; // Future: abort signal support
        let tracker = tracker_stop.clone();
        async move {
            let agent_id = input
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let agent_transcript_path = input
                .get("agent_transcript_path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            info!(
                agent_id = %agent_id,
                transcript = %agent_transcript_path,
                "Subagent stopped"
            );

            // Use std::sync::Mutex - quick lock for Vec push
            {
                let mut t = tracker.lock().unwrap();
                t.stopped.push(SubagentInfo {
                    agent_id,
                    agent_type: "completed".to_string(),
                    timestamp: std::time::Instant::now(),
                });
            }

            Ok(HookOutput::default())
        }
    });

    // Build hooks configuration
    let mut hooks = HashMap::new();
    hooks.insert(
        HookEvent::SubagentStart,
        vec![
            HookMatcherBuilder::new(None::<String>)
                .add_hook(subagent_start_hook)
                .build(),
        ],
    );
    hooks.insert(
        HookEvent::SubagentStop,
        vec![
            HookMatcherBuilder::new(None::<String>)
                .add_hook(subagent_stop_hook)
                .build(),
        ],
    );

    // Create HookManager with hooks
    let mut manager = HookManager::from_hooks_config(hooks);

    info!("Testing hook invocation via simulated messages...");

    // Simulate an Assistant message with Task tool use
    let task_message = Message::Assistant {
        parent_tool_use_id: None,
        message: anthropic_agent_sdk::types::AssistantMessageContent::new(
            "claude-sonnet-4-20250514",
            vec![ContentBlock::ToolUse {
                id: "toolu_abc123".to_string(),
                name: "Task".to_string(),
                input: serde_json::json!({
                    "description": "Get current time",
                    "prompt": "Run: echo $(date)",
                    "subagent_type": "general-purpose"
                }),
            }],
        ),
        session_id: None,
        uuid: None,
        error: None,
    };

    // Process the message - should trigger SubagentStart hook
    let outputs = manager.process_message(&task_message).await?;
    debug!(count = outputs.len(), "Hook outputs from Task message");

    // Simulate the Task result coming back
    let result_message = Message::User {
        parent_tool_use_id: None,
        message: anthropic_agent_sdk::types::UserMessageContent {
            role: "user".to_string(),
            content: Some(anthropic_agent_sdk::types::UserContent::Blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "toolu_abc123".to_string(),
                    content: Some(anthropic_agent_sdk::types::ContentValue::String(
                        "Found 15 Rust files".to_string(),
                    )),
                    is_error: Some(false),
                },
            ])),
        },
        session_id: None,
        uuid: None,
        is_replay: false,
        is_synthetic: false,
        timestamp: None,
    };

    // Process the result - should trigger SubagentStop hook
    let outputs = manager.process_message(&result_message).await?;
    debug!(count = outputs.len(), "Hook outputs from result message");

    // Print summary - scope the lock to release before CLI test
    {
        let t = tracker.lock().unwrap();
        info!(
            started = t.started.len(),
            stopped = t.stopped.len(),
            "Simulated test summary"
        );
    }

    // Now test with a real client if available
    info!("--- Testing with real Claude CLI ---");

    let options = ClaudeAgentOptions::builder()
        .max_turns(3)
        .system_prompt(
            "You are a helpful assistant. When asked, use the Task tool to spawn a quick subagent.",
        )
        .build();

    match ClaudeSDKClient::new(options, None).await {
        Ok(mut client) => {
            info!("Client connected, sending prompt...");

            client
                .send_message(
                    "Use the Task tool with subagent_type 'general-purpose' and model 'haiku' to run: echo $(date). Return the result.",
                )
                .await?;

            // Read responses and process them through our hook manager
            loop {
                trace!("Waiting for next message...");
                match tokio::time::timeout(Duration::from_secs(30), client.next_message()).await {
                    Ok(Some(message)) => {
                        match message {
                            Ok(msg) => {
                                // Log message type at appropriate level
                                match &msg {
                                    Message::Assistant {
                                        message,
                                        parent_tool_use_id,
                                        ..
                                    } => {
                                        debug!(
                                            model = %message.model,
                                            blocks = message.content.len(),
                                            parent = ?parent_tool_use_id,
                                            "Assistant message"
                                        );
                                        for block in &message.content {
                                            if let ContentBlock::ToolUse { id, name, .. } = block {
                                                info!(tool = %name, id = %id, "Tool use");
                                            }
                                        }
                                    }
                                    Message::User {
                                        parent_tool_use_id, ..
                                    } => {
                                        trace!(parent = ?parent_tool_use_id, "User message");
                                    }
                                    Message::System { subtype, .. } => {
                                        debug!(subtype = %subtype, "System message");
                                    }
                                    Message::Result {
                                        session_id,
                                        duration_ms,
                                        num_turns,
                                        ..
                                    } => {
                                        info!(
                                            session = %session_id,
                                            duration_ms = duration_ms,
                                            turns = num_turns,
                                            "Session complete"
                                        );
                                    }
                                    Message::StreamEvent { .. } => {
                                        trace!("Stream event");
                                    }
                                    other => {
                                        trace!(kind = other.message_type(), "Other message");
                                    }
                                }

                                // Process through our hook manager
                                let outputs = manager.process_message(&msg).await?;
                                if !outputs.is_empty() {
                                    debug!(count = outputs.len(), "Hooks triggered");
                                }

                                // Check for completion
                                if let Message::Result { .. } = &msg {
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Message error");
                                break;
                            }
                        }
                    }
                    Ok(None) => {
                        debug!("Stream ended");
                        break;
                    }
                    Err(_) => {
                        warn!("Timeout after 30s");
                        break;
                    }
                }
            }

            debug!("Closing client...");
            client.close().await?;

            // Print final summary
            let t = tracker.lock().unwrap();
            info!(
                started = t.started.len(),
                stopped = t.stopped.len(),
                "=== Final Summary ==="
            );
            for (i, info) in t.started.iter().enumerate() {
                info!(
                    index = i + 1,
                    agent_id = %info.agent_id,
                    agent_type = %info.agent_type,
                    age_ms = info.timestamp.elapsed().as_millis(),
                    "Tracked subagent"
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "Could not connect to Claude CLI (expected if not installed)");
        }
    }

    info!("=== Test Complete ===");
    Ok(())
}
