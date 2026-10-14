//! File Checkpointing Protocol Discovery
//!
//! This example is designed to discover the control protocol format for
//! file checkpointing by:
//! 1. Capturing all raw messages and stderr from the CLI
//! 2. Attempting rewind operations to see error responses
//! 3. Logging everything for protocol analysis
//!
//! Run with: RUST_LOG=debug cargo run --example checkpointing_demo
//!
//! Or with CLI debug: claude-test -d "api,protocol" -p "test"

use anthropic_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient, Message, SettingSource};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing to capture SDK internals
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "anthropic_agent_sdk=debug,checkpointing_demo=debug"
                    .parse()
                    .unwrap()
            }),
        )
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    println!("=== File Checkpointing Protocol Discovery ===\n");

    // Create temp directory for test
    let temp_dir = env::temp_dir().join("claude_checkpoint_test");
    std::fs::create_dir_all(&temp_dir)?;
    println!("Working directory: {}\n", temp_dir.display());

    // Capture stderr from CLI for protocol debugging
    let stderr_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_log_clone = stderr_log.clone();

    let stderr_callback: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |data: String| {
        // Log all stderr output
        eprintln!("[CLI STDERR] {}", data.trim());
        if let Ok(mut log) = stderr_log_clone.lock() {
            log.push(data);
        }
    });

    // Configure with checkpointing and stderr capture
    let options = ClaudeAgentOptions::builder()
        .enable_file_checkpointing(true)
        .cwd(temp_dir.clone())
        .model("haiku")
        .max_turns(3u32)
        .setting_sources(vec![
            SettingSource::User,
            SettingSource::Project,
            SettingSource::Local,
        ])
        .stderr(stderr_callback)
        .build();

    println!("Options configured:");
    println!("  enable_file_checkpointing: true");
    println!("  model: haiku");
    println!("  cwd: {}\n", temp_dir.display());

    let mut client = ClaudeSDKClient::new(options, None).await?;

    // Track checkpoint UUIDs from user messages
    let mut checkpoints: Vec<String> = Vec::new();
    let mut session_id: Option<String> = None;

    // Send a simple prompt to capture messages
    println!("--- Sending initial prompt ---\n");
    client.send_message("What is 2+2? Reply briefly.").await?;

    // Process all messages, logging raw data
    loop {
        match tokio::time::timeout(Duration::from_secs(30), client.next_message()).await {
            Ok(Some(result)) => {
                match result {
                    Ok(msg) => {
                        // Log raw message for protocol analysis
                        tracing::debug!(message = ?msg, "Raw message received");

                        match &msg {
                            Message::User { uuid, message, .. } => {
                                println!("[USER] content: {:?}", message);
                                if let Some(u) = uuid {
                                    println!("[USER] uuid: {}", u);
                                    checkpoints.push(u.clone());
                                } else {
                                    println!("[USER] uuid: None");
                                }
                            }
                            Message::Assistant { message, .. } => {
                                for block in &message.content {
                                    match block {
                                        anthropic_agent_sdk::ContentBlock::Text { text } => {
                                            println!("[ASSISTANT] {}", text);
                                        }
                                        anthropic_agent_sdk::ContentBlock::ToolUse {
                                            name,
                                            input,
                                            ..
                                        } => {
                                            println!("[TOOL_USE] {} input: {}", name, input);
                                        }
                                        other => {
                                            println!("[BLOCK] {:?}", other);
                                        }
                                    }
                                }
                            }
                            Message::System { subtype, data } => {
                                println!("[SYSTEM] subtype: {}", subtype);
                                tracing::debug!(subtype, data = %data, "System message");
                            }
                            Message::Result {
                                session_id: sid,
                                subtype,
                                ..
                            } => {
                                println!("[RESULT] session: {}, subtype: {}", sid, subtype);
                                session_id = Some(sid.to_string());
                                break;
                            }
                            Message::StreamEvent { event, .. } => {
                                tracing::trace!(event = %event, "Stream event");
                            }
                            other => {
                                tracing::debug!(kind = other.message_type(), "Other message");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Message error: {}", e);
                        tracing::error!(error = %e, "Message processing error");
                        break;
                    }
                }
            }
            Ok(None) => {
                println!("[STREAM] End of stream");
                break;
            }
            Err(_) => {
                println!("[TIMEOUT] No message in 30s");
                break;
            }
        }
    }

    // Summary
    println!("\n--- Checkpoint Summary ---");
    println!("Session ID: {:?}", session_id);
    println!("Checkpoints captured: {}", checkpoints.len());
    for (i, uuid) in checkpoints.iter().enumerate() {
        println!("  {}: {}", i + 1, uuid);
    }

    // Attempt rewind if we have a checkpoint
    if let Some(uuid) = checkpoints.first() {
        println!("\n--- Testing rewind_files() ---");
        println!("Sending rewind request for UUID: {}", uuid);

        // Note: rewind_files() sends a control_request to the CLI.
        // The CLI responds with a control_response (not a regular Message).
        // Look for "Received control_response" in debug output to confirm delivery.
        match client.rewind_files(uuid).await {
            Ok(()) => {
                println!("[OK] Rewind request sent (check debug output for control_response)");
                // Give time for the control_response to be processed
                tokio::time::sleep(Duration::from_millis(500)).await;
                println!("[INFO] Control response handled internally by SDK");
            }
            Err(e) => {
                eprintln!("[ERROR] Rewind failed: {}", e);
                tracing::error!(error = %e, "Rewind request failed");
            }
        }
    } else {
        println!("\n[SKIP] No checkpoints captured - UUID field may not be populated");
        println!(
            "This likely means --replay-user-messages is needed or checkpointing isn't active"
        );
    }

    // Print captured stderr
    println!("\n--- CLI Stderr Log ---");
    if let Ok(log) = stderr_log.lock() {
        if log.is_empty() {
            println!("(empty)");
        } else {
            for line in log.iter() {
                println!("{}", line.trim());
            }
        }
    }

    client.close().await?;

    println!("\n=== Protocol Discovery Complete ===");
    println!("Check the debug output above for protocol format details.");
    println!("Run with RUST_LOG=trace for more verbose output.");

    Ok(())
}
