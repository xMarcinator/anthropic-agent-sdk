//! Test custom `AgentDefinition` - define and invoke custom agents
//!
//! This example demonstrates:
//! - Defining custom agents via ClaudeAgentOptions.agents
//! - Invoking custom agents via the Task tool with `subagent_type`
//! - Tracking custom agent lifecycle with hooks
//!
//! Run with: cargo run --example `custom_agent_test`
//! Run with debug: `RUST_LOG=debug` cargo run --example `custom_agent_test`

use anthropic_agent_sdk::ClaudeSDKClient;
use anthropic_agent_sdk::hooks::{HookManager, HookMatcherBuilder};
use anthropic_agent_sdk::types::{
    AgentDefinition, ClaudeAgentOptions, HookEvent, HookOutput, Message,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// State to track custom agent usage
#[derive(Debug, Default)]
struct AgentTracker {
    started: Vec<(String, String)>, // (agent_id, agent_type)
    stopped: Vec<String>,           // agent_id
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .with_ansi(true)
        .init();

    info!("=== Custom Agent Definition Test ===");

    // Shared state to track agents
    let tracker = Arc::new(Mutex::new(AgentTracker::default()));

    // Create SubagentStart hook to track custom agents
    let tracker_start = tracker.clone();
    let subagent_start_hook = HookManager::callback(move |input, _tool_name, ctx| {
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
                session = ?ctx.session_id,
                "Custom agent started"
            );

            {
                let mut t = tracker.lock().unwrap();
                t.started.push((agent_id, agent_type));
            }

            Ok(HookOutput::default())
        }
    });

    // Create SubagentStop hook
    let tracker_stop = tracker.clone();
    let subagent_stop_hook = HookManager::callback(move |input, _tool_name, ctx| {
        let tracker = tracker_stop.clone();
        async move {
            let agent_id = input
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            info!(agent_id = %agent_id, cwd = ?ctx.cwd, "Custom agent stopped");

            {
                let mut t = tracker.lock().unwrap();
                t.stopped.push(agent_id);
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

    // Define a custom agent
    let mut agents = HashMap::new();
    agents.insert(
        "time-checker".to_string(),
        AgentDefinition {
            description: "Agent that checks and reports the current time".to_string(),
            prompt: "You are a time-checking agent. When invoked, run `date` to get the current time and return it in a friendly format. Be concise.".to_string(),
            tools: Some(vec!["Bash".to_string()]),
            model: Some("haiku".to_string()),
        },
    );

    info!(agent_count = agents.len(), "Defined custom agents");
    for (name, def) in &agents {
        info!(
            name = %name,
            description = %def.description,
            model = ?def.model,
            "Agent definition"
        );
    }

    // Create client with custom agents and hooks
    let options = ClaudeAgentOptions::builder()
        .max_turns(5)
        .system_prompt("You are a helpful assistant. You have access to a custom agent called 'time-checker' that you can invoke via the Task tool.")
        .agents(agents)
        .hooks(hooks)
        .build();

    match ClaudeSDKClient::new(options, None).await {
        Ok(mut client) => {
            info!("Client connected with custom agent definition");

            // Send a message that will invoke our custom agent
            client
                .send_message(
                    "Use the Task tool to spawn a 'time-checker' agent. Set subagent_type to 'time-checker'. Return what it reports.",
                )
                .await?;

            // Read responses
            loop {
                match tokio::time::timeout(Duration::from_secs(60), client.next_message()).await {
                    Ok(Some(message)) => match message? {
                        Message::Assistant { message, .. } => {
                            for block in &message.content {
                                match block {
                                    anthropic_agent_sdk::types::ContentBlock::Text { text } => {
                                        let preview = if text.len() > 150 {
                                            format!("{}...", &text[..150])
                                        } else {
                                            text.clone()
                                        };
                                        info!("Assistant: {}", preview);
                                    }
                                    anthropic_agent_sdk::types::ContentBlock::ToolUse {
                                        name,
                                        input,
                                        ..
                                    } if name == "Task" => {
                                        let subagent_type = input
                                            .get("subagent_type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("unknown");
                                        info!(
                                            tool = %name,
                                            subagent_type = %subagent_type,
                                            "Task tool invoked"
                                        );
                                    }
                                    _ => {}
                                }
                            }
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
                            break;
                        }
                        _ => {}
                    },
                    Ok(None) => {
                        debug!("Stream ended");
                        break;
                    }
                    Err(_) => {
                        warn!("Timeout after 60s");
                        break;
                    }
                }
            }

            client.close().await?;

            // Print final summary
            let t = tracker.lock().unwrap();
            info!(
                started = t.started.len(),
                stopped = t.stopped.len(),
                "=== Final Summary ==="
            );

            // Check if our custom agent was used
            let custom_agent_used = t
                .started
                .iter()
                .any(|(_, agent_type)| agent_type == "time-checker");
            if custom_agent_used {
                info!("✓ Custom 'time-checker' agent was successfully invoked!");
            } else {
                warn!("Custom agent was not invoked. Agent types seen:");
                for (id, agent_type) in &t.started {
                    info!(agent_id = %id, agent_type = %agent_type, "Subagent");
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "Could not connect to Claude CLI");
        }
    }

    info!("=== Test Complete ===");
    Ok(())
}
