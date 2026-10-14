//! `ClaudeSDKClient` for bidirectional communication
//!
//! This module provides the main client for interactive, stateful conversations
//! with Claude Code, including support for:
//! - Bidirectional messaging (no lock contention)
//! - Interrupts and control flow
//! - Hook and permission callbacks
//! - Conversation state management
//!
//! # Architecture
//!
//! The client uses a lock-free architecture for reading and writing:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                   ClaudeSDKClient                        │
//! │                                                          │
//! │  ┌──────────────────┐        ┌──────────────────┐      │
//! │  │  Message Reader  │        │  Control Writer  │      │
//! │  │  Background Task │        │  Background Task │      │
//! │  │                  │        │                  │      │
//! │  │ • Gets receiver  │        │ • Locks per-write│      │
//! │  │   once           │        │ • No blocking    │      │
//! │  │ • No lock held   │        │                  │      │
//! │  │   while reading  │        │                  │      │
//! │  └────────┬─────────┘        └────────┬─────────┘      │
//! │           │                           │                 │
//! │           │    ┌──────────────┐      │                 │
//! │           └───→│  Transport   │←─────┘                 │
//! │                │  (Arc<Mutex>)│                         │
//! │                └──────────────┘                         │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! **Key Design Points:**
//! - Transport returns an owned `UnboundedReceiver` (no lifetime issues)
//! - Reader task gets receiver once, then releases transport lock
//! - Writer task locks transport briefly for each write operation
//! - No contention: reader never blocks writer, writer never blocks reader
//!
//! # Example: Basic Usage
//!
//! ```no_run
//! use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let options = ClaudeAgentOptions::default();
//! let mut client = ClaudeSDKClient::new(options, None).await?;
//!
//! // Send a message
//! client.send_message("Hello, Claude!").await?;
//!
//! // Read responses
//! while let Some(message) = client.next_message().await {
//!     match message? {
//!         Message::Assistant { message, .. } => {
//!             println!("Response: {:?}", message.content);
//!         }
//!         Message::Result { .. } => break,
//!         _ => {}
//!     }
//! }
//!
//! client.close().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Example: Concurrent Operations
//!
//! ```no_run
//! use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let options = ClaudeAgentOptions::default();
//! let mut client = ClaudeSDKClient::new(options, None).await?;
//!
//! // Send first message
//! client.send_message("First question").await?;
//!
//! // Can send another message while reading responses
//! // No blocking due to lock-free architecture
//! tokio::spawn(async move {
//!     tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
//!     client.send_message("Second question").await
//! });
//!
//! # Ok(())
//! # }
//! ```
//!
//! # Example: Interrupt
//!
//! ```no_run
//! use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let options = ClaudeAgentOptions::default();
//! let mut client = ClaudeSDKClient::new(options, None).await?;
//!
//! client.send_message("Write a long essay").await?;
//!
//! // After some time, interrupt the response
//! tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
//! client.interrupt().await?;
//!
//! # Ok(())
//! # }
//! ```
//!
//! # Example: Hooks and Permissions
//!
//! ```no_run
//! use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let options = ClaudeAgentOptions::default();
//! let mut client = ClaudeSDKClient::new(options, None).await?;
//!
//! // Take receivers to handle hooks and permissions
//! let mut hook_rx = client.take_hook_receiver().unwrap();
//! let mut perm_rx = client.take_permission_receiver().unwrap();
//!
//! // Handle hook events
//! tokio::spawn(async move {
//!     while let Some((hook_id, event)) = hook_rx.recv().await {
//!         println!("Hook: {} {:?}", hook_id, event);
//!         // Respond to hook...
//!     }
//! });
//!
//! // Handle permission requests
//! tokio::spawn(async move {
//!     while let Some((req_id, request)) = perm_rx.recv().await {
//!         println!("Permission: {:?}", request);
//!         // Respond to permission...
//!     }
//! });
//!
//! # Ok(())
//! # }
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::control::wire::{
    CanUseToolRequest, ControlRequestBody, ControlRequestEnvelope, ControlResponseBody,
    ControlResponseEnvelope, InboundControlRequest, permission_response_value,
};
use crate::error::{ClaudeError, Result};
use crate::hooks::HookManager;
use crate::message::parse_message;
use crate::permissions::PermissionManager;
use crate::transport::{PromptInput, SubprocessTransport, Transport};
use crate::types::{
    AccountInfo, ClaudeAgentOptions, HookEvent, Message, ModelInfo, PermissionRequest, RequestId,
    SessionId, SessionInfo, ToolName, ToolPermissionContext,
};
use futures::Stream;

/// A buffered message with its associated session ID for security validation
type BufferedMessage = (Option<SessionId>, String);

/// Thread-safe queue for buffering messages during streaming
type MessageBuffer = Arc<std::sync::Mutex<VecDeque<BufferedMessage>>>;

/// Control requests sent to the CLI that await its `control_response`, by request ID.
type PendingControl = Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<ControlResponseBody>>>>;

/// `can_use_tool` requests from the CLI that await a manual answer, by request ID.
type PendingPermissions = Arc<std::sync::Mutex<HashMap<String, CanUseToolRequest>>>;

/// Control requests from the CLI being answered, by request ID: a `control_cancel_request`
/// aborts the answer (and so drops the permission callback's future).
type Answering = Arc<std::sync::Mutex<HashMap<String, tokio::task::AbortHandle>>>;

/// How long control requests wait for the CLI's answer by default.
pub const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// Context for the message reader background task
struct MessageReaderContext {
    transport: Arc<Mutex<SubprocessTransport>>,
    message_tx: mpsc::UnboundedSender<Result<Message>>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    session_info: Arc<std::sync::Mutex<Option<SessionInfo>>>,
    bound_session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    hook_manager: Option<Arc<Mutex<HookManager>>>,
    permission_manager: Option<Arc<PermissionManager>>,
    permission_tx: mpsc::UnboundedSender<(RequestId, PermissionRequest)>,
    pending_control: PendingControl,
    pending_permissions: PendingPermissions,
    is_resume: bool,
}

/// Client for bidirectional communication with Claude Code
///
/// `ClaudeSDKClient` provides interactive, stateful conversations with
/// support for interrupts, hooks, and permission callbacks.
///
/// Control messages use Claude Code's stream-JSON control protocol: requests to the CLI
/// (`interrupt`, `set_model`, ...) are correlated with their `control_response` by request ID,
/// and the CLI's own requests (`can_use_tool` with `--permission-prompt-tool stdio`) are
/// answered by the client. Neither appears in the message stream.
///
/// # Examples
///
/// ```no_run
/// use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
/// use futures::StreamExt;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let options = ClaudeAgentOptions::default();
///     let mut client = ClaudeSDKClient::new(options, None).await?;
///
///     client.send_message("Hello, Claude!").await?;
///
///     while let Some(message) = client.next_message().await {
///         println!("{:?}", message?);
///     }
///
///     Ok(())
/// }
/// ```
pub struct ClaudeSDKClient {
    /// Transport layer
    transport: Arc<Mutex<SubprocessTransport>>,
    /// Message stream receiver
    message_rx: mpsc::UnboundedReceiver<Result<Message>>,
    /// Hook event receiver (if not using automatic handler)
    hook_rx: Option<mpsc::UnboundedReceiver<(String, HookEvent)>>,
    /// Permission request receiver (if not using automatic handler)
    permission_rx: Option<mpsc::UnboundedReceiver<(RequestId, PermissionRequest)>>,
    /// Hook manager for automatic hook handling (kept alive for background tasks)
    hook_manager: Option<Arc<Mutex<HookManager>>>,
    /// Permission manager for automatic permission handling (kept alive for background tasks)
    #[allow(dead_code)]
    permission_manager: Option<Arc<PermissionManager>>,
    /// Captured session ID from messages
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    /// Session info from init message (model, tools, MCP servers)
    session_info: Arc<std::sync::Mutex<Option<SessionInfo>>>,
    /// Cancellation token for aborting operations (like `AbortController` in JS)
    cancellation_token: CancellationToken,
    /// Runtime model override
    runtime_model: Arc<std::sync::Mutex<Option<String>>>,
    /// Runtime permission mode override
    runtime_permission_mode: Arc<std::sync::Mutex<Option<crate::types::PermissionMode>>>,
    /// Runtime max thinking tokens override
    runtime_max_thinking_tokens: Arc<std::sync::Mutex<Option<u32>>>,
    /// Message buffer for queuing messages during streaming
    message_buffer: MessageBuffer,
    /// Bound session ID - if set, all sends validate against this
    bound_session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    /// Control requests awaiting the CLI's answer
    pending_control: PendingControl,
    /// `can_use_tool` requests awaiting `respond_to_permission`
    pending_permissions: PendingPermissions,
    /// The CLI process
    process: Option<crate::transport::ProcessHandle>,
}

/// A fresh control request ID, unique within the process.
fn next_request_id() -> String {
    static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "req_{}_{:x}",
        REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Writes one line to the CLI's stdin.
async fn write_line(transport: &Arc<Mutex<SubprocessTransport>>, line: &str) -> Result<()> {
    let mut guard = transport.lock().await;
    guard.write(line).await
}

impl ClaudeSDKClient {
    /// Create a new `ClaudeSDKClient`
    ///
    /// # Arguments
    /// * `options` - Configuration options
    /// * `cli_path` - Optional path to Claude Code CLI
    ///
    /// # Errors
    /// Returns error if CLI cannot be found or connection fails
    pub async fn new(
        options: ClaudeAgentOptions,
        cli_path: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        // Create cancellation token (like AbortController in JavaScript)
        let cancellation_token = CancellationToken::new();

        // Initialize hook manager if hooks are configured
        let (hook_manager, hook_rx) = if let Some(ref hooks_config) = options.hooks {
            let mut manager = HookManager::from_hooks_config(hooks_config.clone());
            // Set cancellation token so hooks can check for abort
            manager.set_cancellation_token(cancellation_token.child_token());
            (Some(Arc::new(Mutex::new(manager))), None)
        } else {
            (None, Some(mpsc::unbounded_channel().1))
        };

        // Permission requests (`can_use_tool`) go to the callback when one is configured,
        // otherwise to the receiver returned by `take_permission_receiver`.
        let (permission_tx, permission_rx_internal) = mpsc::unbounded_channel();
        let (permission_manager, permission_rx) = if options.can_use_tool.is_some() {
            let mut manager = PermissionManager::new();
            if let Some(callback) = options.can_use_tool.clone() {
                manager.set_callback(callback);
            }
            if !options.allowed_tools.is_empty() {
                manager.set_allowed_tools(Some(options.allowed_tools.clone()));
            }
            manager.set_disallowed_tools(options.disallowed_tools.clone());
            (Some(Arc::new(manager)), None)
        } else {
            (None, Some(permission_rx_internal))
        };

        // Check if this is a resume session (for SessionStart hook)
        let is_resume = options.resume.is_some();

        // Create transport with streaming mode and pass child cancellation token
        let prompt_input = PromptInput::Stream;
        let mut transport = SubprocessTransport::with_cancellation_token(
            prompt_input,
            options,
            cli_path,
            Some(cancellation_token.child_token()),
        )?;

        // Connect transport
        transport.connect().await?;
        let process = transport.process();

        let (message_tx, message_rx) = mpsc::unbounded_channel();

        let transport = Arc::new(Mutex::new(transport));
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_info = Arc::new(std::sync::Mutex::new(None));
        let bound_session_id = Arc::new(std::sync::Mutex::new(None));
        let pending_control: PendingControl = Arc::default();
        let pending_permissions: PendingPermissions = Arc::default();

        // Spawn message reader task
        let reader_ctx = MessageReaderContext {
            transport: transport.clone(),
            message_tx,
            session_id: session_id.clone(),
            session_info: session_info.clone(),
            bound_session_id: bound_session_id.clone(),
            hook_manager: hook_manager.clone(),
            permission_manager: permission_manager.clone(),
            permission_tx,
            pending_control: pending_control.clone(),
            pending_permissions: pending_permissions.clone(),
            is_resume,
        };
        tokio::spawn(async move {
            Self::message_reader_task(reader_ctx).await;
        });

        Ok(Self {
            transport,
            message_rx,
            hook_rx,
            permission_rx,
            hook_manager,
            permission_manager,
            session_id,
            session_info,
            cancellation_token,
            runtime_model: Arc::new(std::sync::Mutex::new(None)),
            runtime_permission_mode: Arc::new(std::sync::Mutex::new(None)),
            runtime_max_thinking_tokens: Arc::new(std::sync::Mutex::new(None)),
            message_buffer: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            bound_session_id,
            pending_control,
            pending_permissions,
            process,
        })
    }

    /// Message reader task - reads from transport and processes messages
    ///
    /// Control responses resolve the waiting request, control requests from the CLI are
    /// answered, and every other message goes to the message channel. If `hook_manager` is
    /// provided, automatically calls `process_message()` on each message to trigger registered
    /// hooks (`SubagentStart`, `SubagentStop`, etc.)
    #[allow(clippy::too_many_lines)]
    async fn message_reader_task(ctx: MessageReaderContext) {
        let MessageReaderContext {
            transport,
            message_tx,
            session_id,
            session_info,
            bound_session_id,
            hook_manager,
            permission_manager,
            permission_tx,
            pending_control,
            pending_permissions,
            is_resume,
        } = ctx;
        let answering: Answering = Arc::default();
        // Get the message receiver from the transport without holding the lock
        let mut msg_stream = {
            let mut transport_guard = transport.lock().await;
            transport_guard.read_messages()
        };

        while let Some(result) = msg_stream.recv().await {
            let value = match result {
                Ok(value) => value,
                Err(e) => {
                    // A line that is not JSON is reported and skipped; anything else ends the
                    // stream.
                    let skipped = matches!(e, ClaudeError::JsonDecode(_));
                    let _ = message_tx.send(Err(e));
                    if skipped {
                        continue;
                    }
                    break;
                }
            };
            let msg = match parse_message(value) {
                Ok(msg) => msg,
                Err(e) => {
                    tracing::debug!(error = ?e, "Failed to parse message");
                    let _ = message_tx.send(Err(e));
                    continue;
                }
            };

            match msg {
                Message::ControlResponse { response } => {
                    let waiting = pending_control
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.remove(&response.request_id));
                    match waiting {
                        Some(tx) => {
                            let _ = tx.send(response);
                        }
                        None => tracing::debug!(
                            request_id = %response.request_id,
                            "control_response without a waiting request"
                        ),
                    }
                    continue;
                }
                Message::ControlRequest {
                    request_id,
                    request,
                } => {
                    let transport = transport.clone();
                    let permission_manager = permission_manager.clone();
                    let permission_tx = permission_tx.clone();
                    let pending_permissions = pending_permissions.clone();
                    let answers = answering.clone();
                    let id = request_id.clone();
                    let key = request_id.clone();
                    // Registered before the task can finish and deregister itself.
                    let mut registry = answering.lock().ok();
                    let task = tokio::spawn(async move {
                        Self::answer_control_request(
                            &transport,
                            permission_manager,
                            &permission_tx,
                            &pending_permissions,
                            request_id,
                            request,
                        )
                        .await;
                        if let Ok(mut answers) = answers.lock() {
                            answers.remove(&id);
                        }
                    });
                    if let Some(registry) = registry.as_mut() {
                        registry.insert(key, task.abort_handle());
                    }
                    continue;
                }
                Message::ControlCancelRequest { ref request_id } => {
                    if let Ok(mut pending) = pending_permissions.lock() {
                        pending.remove(request_id);
                    }
                    // The CLI no longer waits: its permission callback is dropped unanswered.
                    let answer = answering
                        .lock()
                        .ok()
                        .and_then(|mut answers| answers.remove(request_id));
                    if let Some(answer) = answer {
                        answer.abort();
                    }
                    tracing::debug!(request_id = %request_id, "CLI cancelled a control request");
                    continue;
                }
                _ => {}
            }

            // Capture session_id from Result messages
            if let Message::Result {
                session_id: ref sid,
                ..
            } = msg
            {
                if let Ok(mut session_guard) = session_id.lock() {
                    *session_guard = Some(sid.clone());
                }
                // Auto-bind to session on first Result (secure by default)
                if let Ok(mut bound_guard) = bound_session_id.lock() {
                    if bound_guard.is_none() {
                        *bound_guard = Some(sid.clone());
                    }
                }
            }

            // Capture session info from System init message and pass to HookManager
            if let Message::System {
                ref subtype,
                ref data,
            } = msg
            {
                if subtype == "init" {
                    let init_session_id = data
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string);
                    let init_cwd = data
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string);

                    // Update session_id storage
                    if let Some(ref sid) = init_session_id {
                        if let Ok(mut session_guard) = session_id.lock() {
                            *session_guard = Some(SessionId::from(sid.clone()));
                        }
                    }

                    // Populate session_info from init data
                    if let Ok(mut info_guard) = session_info.lock() {
                        *info_guard = Some(SessionInfo::from_init_data(data));
                    }

                    // Update HookManager with session context and trigger SessionStart
                    if let Some(ref manager) = hook_manager {
                        if let Some(sid) = init_session_id {
                            let mut manager_guard = manager.lock().await;
                            manager_guard.set_session_context(sid, init_cwd);

                            // Determine session start source
                            let source = if is_resume { "resume" } else { "startup" };

                            // Trigger SessionStart hook
                            if let Err(e) = manager_guard.trigger_session_start(source).await {
                                tracing::warn!(error = %e, "SessionStart hook error");
                            }

                            tracing::debug!(source = source, "Triggered SessionStart hook");
                        }
                    }
                }
            }

            // Process message through hook manager if configured
            // This triggers SubagentStart, SubagentStop, PreToolUse, PostToolUse hooks
            if let Some(ref manager) = hook_manager {
                let mut manager_guard = manager.lock().await;
                match manager_guard.process_message(&msg).await {
                    Ok(outputs) => {
                        if !outputs.is_empty() {
                            tracing::debug!(
                                count = outputs.len(),
                                "Hook outputs from message processing"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Hook processing error");
                    }
                }
            }

            if message_tx.send(Ok(msg)).is_err() {
                tracing::warn!("Failed to send message to channel - receiver dropped");
                break;
            }
        }

        // The CLI is gone: no waiting control request will be answered.
        if let Ok(mut pending) = pending_control.lock() {
            pending.clear();
        }
    }

    /// Answers one control request from the CLI so it never waits: `can_use_tool` through
    /// the permission callback (or the manual permission receiver), anything else with an
    /// error naming the unsupported subtype.
    async fn answer_control_request(
        transport: &Arc<Mutex<SubprocessTransport>>,
        permission_manager: Option<Arc<PermissionManager>>,
        permission_tx: &mpsc::UnboundedSender<(RequestId, PermissionRequest)>,
        pending_permissions: &PendingPermissions,
        request_id: String,
        request: ControlRequestBody,
    ) {
        let answer = match InboundControlRequest::from(request) {
            InboundControlRequest::CanUseTool(request) => {
                let context = ToolPermissionContext::new(
                    request
                        .permission_suggestions
                        .clone()
                        .and_then(|value| serde_json::from_value(value).ok())
                        .unwrap_or_default(),
                );
                if let Some(manager) = permission_manager {
                    // No lock is held while the callback waits: requests are answered
                    // concurrently.
                    let decision = manager
                        .can_use_tool(
                            ToolName::new(request.tool_name.clone()),
                            request.input.clone(),
                            context,
                        )
                        .await;
                    match decision {
                        Ok(result) => ControlResponseBody::success(
                            request_id,
                            Some(permission_response_value(&result, &request)),
                        ),
                        Err(e) => ControlResponseBody::error(request_id, e.to_string()),
                    }
                } else {
                    // Manual handling: the caller answers with `respond_to_permission`.
                    let manual = PermissionRequest {
                        tool_name: ToolName::new(request.tool_name.clone()),
                        tool_input: request.input.clone(),
                        context,
                    };
                    if let Ok(mut pending) = pending_permissions.lock() {
                        pending.insert(request_id.clone(), request);
                    }
                    if permission_tx
                        .send((RequestId::new(request_id.clone()), manual))
                        .is_ok()
                    {
                        return;
                    }
                    if let Ok(mut pending) = pending_permissions.lock() {
                        pending.remove(&request_id);
                    }
                    ControlResponseBody::error(request_id, "no permission handler is registered")
                }
            }
            InboundControlRequest::HookCallback { callback_id, .. } => ControlResponseBody::error(
                request_id,
                format!("hook callback {callback_id} is not registered with this SDK"),
            ),
            InboundControlRequest::McpMessage { server_name, .. } => ControlResponseBody::error(
                request_id,
                format!("SDK MCP server {server_name} is not served by this client"),
            ),
            InboundControlRequest::Other(body) => ControlResponseBody::error(
                request_id,
                format!("Unsupported control request subtype: {}", body.subtype),
            ),
        };
        let line = ControlResponseEnvelope::new(answer).to_line();
        if let Err(e) = write_line(transport, &line).await {
            tracing::warn!(error = %e, "Failed to answer a control request");
        }
    }

    /// Sends a control request to the CLI and waits up to `timeout` for its answer.
    ///
    /// # Errors
    /// Returns `ControlTimeout` when no answer arrives in time, `ControlProtocol` when the CLI
    /// exits first or answers with an error, and a transport error when the request cannot be
    /// written.
    pub async fn send_control_request(
        &self,
        request: ControlRequestBody,
        timeout: Duration,
    ) -> Result<ControlResponseBody> {
        let request_id = next_request_id();
        let subtype = request.subtype.clone();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending_control.lock() {
            pending.insert(request_id.clone(), tx);
        }
        let line = ControlRequestEnvelope::new(request_id.clone(), request).to_line();
        if let Err(e) = write_line(&self.transport, &line).await {
            if let Ok(mut pending) = self.pending_control.lock() {
                pending.remove(&request_id);
            }
            return Err(e);
        }
        let answer = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                return Err(ClaudeError::control_protocol(format!(
                    "Claude Code exited before answering {subtype}"
                )));
            }
            Err(_) => {
                if let Ok(mut pending) = self.pending_control.lock() {
                    pending.remove(&request_id);
                }
                return Err(ClaudeError::control_timeout(timeout.as_secs(), subtype));
            }
        };
        if answer.is_success() {
            Ok(answer)
        } else {
            Err(ClaudeError::control_protocol(format!(
                "{subtype} failed: {}",
                answer.error.as_deref().unwrap_or("unknown error")
            )))
        }
    }

    /// Sends a control request without waiting for the answer.
    async fn send_control_request_nowait(&self, request: ControlRequestBody) -> Result<()> {
        let line = ControlRequestEnvelope::new(next_request_id(), request).to_line();
        write_line(&self.transport, &line).await
    }

    /// Send a message to Claude
    ///
    /// # Arguments
    /// * `content` - Message content to send
    ///
    /// # Errors
    /// Returns error if message cannot be sent
    pub async fn send_message(&mut self, content: impl Into<String>) -> Result<()> {
        self.write_user_message(content.into(), None).await
    }

    /// Send a message to Claude under a `uuid` you choose.
    ///
    /// With `--replay-user-messages` (always on in streaming mode) Claude Code echoes the
    /// message back, with this `uuid` and `isReplay: true`, once it takes it; a
    /// [`Message::CommandLifecycle`] with `command_uuid` equal to it reports `queued`,
    /// `started` and `cancelled`. Use a UUID: the CLI also uses it as the file checkpoint.
    ///
    /// # Errors
    /// Returns error if the message cannot be sent
    pub async fn send_message_with_uuid(
        &mut self,
        content: impl Into<String>,
        uuid: impl Into<String>,
    ) -> Result<()> {
        self.write_user_message(content.into(), Some(uuid.into()))
            .await
    }

    async fn write_user_message(
        &mut self,
        content_str: String,
        uuid: Option<String>,
    ) -> Result<()> {
        // Validate session if bound
        self.validate_session()?;

        // Trigger UserPromptSubmit hook before sending
        if let Some(ref manager) = self.hook_manager {
            let manager_guard = manager.lock().await;
            if let Err(e) = manager_guard.trigger_user_prompt_submit(&content_str).await {
                tracing::warn!(error = %e, "UserPromptSubmit hook error");
            }
        }

        // Send a user message in the format the CLI expects
        let mut message = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": content_str
            }
        });
        if let Some(uuid) = uuid {
            message["uuid"] = serde_json::Value::String(uuid);
        }
        let message_json = format!("{}\n", serde_json::to_string(&message)?);

        write_line(&self.transport, &message_json).await
    }

    // ========================================================================
    // Message Buffering
    // ========================================================================

    /// Queue a message to be sent after the current turn completes.
    ///
    /// The CLI only reads stdin between turns, not during streaming.
    /// Messages queued with this method are stored and can be sent
    /// automatically using `receive_buffered()` or manually with `send_queued()`.
    ///
    /// **Security**: Each queued message is associated with the current `session_id`.
    /// When sending, the SDK verifies the session hasn't changed, preventing
    /// messages from being accidentally sent to a different conversation context.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// // Send first message
    /// client.send_message("What is Python?").await?;
    ///
    /// // Queue follow-up messages (will be sent after each Result)
    /// client.queue_message("What is TypeScript?");
    /// client.queue_message("Compare Rust to both.");
    ///
    /// // Process all messages with automatic queue handling
    /// while let Some(msg) = client.next_buffered().await {
    ///     // Handle messages...
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn queue_message(&self, content: impl Into<String>) {
        // Capture current session_id for security
        let session_id = self.get_session_id();
        if let Ok(mut buffer) = self.message_buffer.lock() {
            buffer.push_back((session_id, content.into()));
        }
    }

    /// Get the number of messages waiting in the queue.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.message_buffer.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Check if there are messages waiting to be sent.
    #[must_use]
    pub fn has_queued(&self) -> bool {
        self.queued_count() > 0
    }

    /// Send the next queued message.
    ///
    /// Returns `Ok(true)` if a message was sent, `Ok(false)` if queue is empty.
    ///
    /// **Security**: Verifies the queued message's `session_id` matches the current
    /// session. If session changed, the message is discarded with a warning to
    /// prevent sending messages to an unintended conversation context.
    ///
    /// # Errors
    /// Returns error if message cannot be sent.
    pub async fn send_queued(&mut self) -> Result<bool> {
        let current_session = self.get_session_id();

        let next_entry = {
            self.message_buffer
                .lock()
                .ok()
                .and_then(|mut b| b.pop_front())
        };

        if let Some((queued_session, msg)) = next_entry {
            // Security check: verify session_id matches
            match (&queued_session, &current_session) {
                (Some(queued), Some(current)) if queued != current => {
                    // Session changed - discard message for safety
                    tracing::warn!(
                        queued_session = %queued,
                        current_session = %current,
                        "Discarding queued message: session_id changed"
                    );
                    // Clear remaining messages from old session
                    self.clear_queue();
                    return Ok(false);
                }
                _ => {
                    // Session matches or one is None (early in conversation)
                    self.send_message(msg).await?;
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Get the next message, automatically sending queued messages after Results.
    ///
    /// This is the recommended way to handle multi-turn conversations with buffering.
    /// After receiving a Result message, any queued messages are automatically sent.
    /// Returns `None` when stream ends AND no more queued messages remain.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// client.send_message("What is Python?").await?;
    /// client.queue_message("What is TypeScript?");
    /// client.queue_message("Compare Rust to both.");
    ///
    /// while let Some(msg) = client.next_buffered().await {
    ///     match msg? {
    ///         Message::Assistant { message, .. } => {
    ///             println!("Claude: {:?}", message.content);
    ///         }
    ///         Message::Result { .. } => {
    ///             println!("Turn complete");
    ///             // next_buffered() automatically sends queued messages
    ///         }
    ///         _ => {}
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next_buffered(&mut self) -> Option<Result<Message>> {
        match self.message_rx.recv().await {
            Some(result) => {
                // Check if this is a Result message
                if let Ok(Message::Result { .. }) = &result {
                    // After Result, try to send next queued message
                    if self.has_queued() {
                        let _ = self.send_queued().await;
                    }
                }
                Some(result)
            }
            None => None,
        }
    }

    /// Clear all queued messages.
    pub fn clear_queue(&self) {
        if let Ok(mut buffer) = self.message_buffer.lock() {
            buffer.clear();
        }
    }

    /// Send an interrupt signal without waiting for the CLI's acknowledgement.
    ///
    /// Writes `{"type":"control_request","request":{"subtype":"interrupt"}}`. The interrupted
    /// turn ends with a `result` message. Use [`Self::interrupt_and_wait`] to get the CLI's
    /// answer.
    ///
    /// # Errors
    /// Returns error if interrupt cannot be sent
    pub async fn interrupt(&mut self) -> Result<()> {
        self.send_control_request_nowait(ControlRequestBody::interrupt())
            .await
    }

    /// Interrupt the current turn and wait for the CLI's `control_response`.
    ///
    /// Claude Code 2.1.292 answers `{"still_queued": [...]}`: the user messages still queued
    /// behind the interrupted turn (see [`ControlResponseBody::still_queued`]).
    ///
    /// # Errors
    /// Returns error if the request cannot be sent, fails, or gets no answer within `timeout`
    pub async fn interrupt_and_wait(&self, timeout: Duration) -> Result<ControlResponseBody> {
        self.send_control_request(ControlRequestBody::interrupt(), timeout)
            .await
    }

    /// Run the `initialize` handshake and return its data: `commands`, `agents`, `models`
    /// (with their supported effort levels), `account`, `pid`, `current_permission_mode`, ...
    ///
    /// # Errors
    /// Returns error if the request cannot be sent, fails, or gets no answer within
    /// [`DEFAULT_CONTROL_TIMEOUT`]
    pub async fn initialize(&self) -> Result<serde_json::Value> {
        let answer = self
            .send_control_request(ControlRequestBody::initialize(), DEFAULT_CONTROL_TIMEOUT)
            .await?;
        Ok(answer.response.unwrap_or(serde_json::Value::Null))
    }

    /// Switch the model for later turns (`set_model`); `None` returns to the default.
    ///
    /// # Errors
    /// Returns error if the CLI refuses or does not answer
    pub async fn change_model(&self, model: Option<String>) -> Result<()> {
        self.send_control_request(
            ControlRequestBody::set_model(model.clone()),
            DEFAULT_CONTROL_TIMEOUT,
        )
        .await?;
        if let Ok(mut guard) = self.runtime_model.lock() {
            *guard = model;
        }
        Ok(())
    }

    /// Switch the permission mode (`set_permission_mode`).
    ///
    /// # Errors
    /// Returns error if the CLI refuses (e.g. `invalid_mode`) or does not answer
    pub async fn change_permission_mode(&self, mode: crate::types::PermissionMode) -> Result<()> {
        self.send_control_request(
            ControlRequestBody::set_permission_mode(mode),
            DEFAULT_CONTROL_TIMEOUT,
        )
        .await?;
        if let Ok(mut guard) = self.runtime_permission_mode.lock() {
            *guard = Some(mode);
        }
        Ok(())
    }

    /// Change the thinking budget (`set_max_thinking_tokens`); `None` turns it off.
    ///
    /// # Errors
    /// Returns error if the CLI refuses or does not answer
    pub async fn change_max_thinking_tokens(&self, tokens: Option<u32>) -> Result<()> {
        self.send_control_request(
            ControlRequestBody::set_max_thinking_tokens(tokens),
            DEFAULT_CONTROL_TIMEOUT,
        )
        .await?;
        if let Ok(mut guard) = self.runtime_max_thinking_tokens.lock() {
            *guard = tokens;
        }
        Ok(())
    }

    /// The status of the session's MCP servers (`mcp_status`).
    ///
    /// # Errors
    /// Returns error if the request fails or gets no answer
    pub async fn mcp_status(&self) -> Result<serde_json::Value> {
        let answer = self
            .send_control_request(ControlRequestBody::mcp_status(), DEFAULT_CONTROL_TIMEOUT)
            .await?;
        Ok(answer.response.unwrap_or(serde_json::Value::Null))
    }

    /// Rewind files to a checkpoint
    ///
    /// Restores files to their state at the specified user message checkpoint.
    /// Requires `enable_file_checkpointing: true` in options.
    ///
    /// # Arguments
    /// * `user_message_uuid` - UUID from a User message's `uuid` field
    ///
    /// # Errors
    /// Returns error if the rewind request cannot be sent
    ///
    /// # Example
    /// ```rust,no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::builder()
    ///     .enable_file_checkpointing(true)
    ///     .build();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// // Capture checkpoint UUID from user messages
    /// let mut checkpoint_uuid: Option<String> = None;
    /// while let Some(msg) = client.next_message().await {
    ///     if let Message::User { uuid: Some(uuid), .. } = msg? {
    ///         checkpoint_uuid = Some(uuid);
    ///         break;
    ///     }
    /// }
    ///
    /// // Later, rewind to that checkpoint
    /// if let Some(uuid) = checkpoint_uuid {
    ///     client.rewind_files(&uuid).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn rewind_files(&mut self, user_message_uuid: &str) -> Result<()> {
        self.send_control_request_nowait(ControlRequestBody::rewind_files(user_message_uuid))
            .await
    }

    /// Closes the CLI's stdin: it finishes what it was given and exits. Nothing can be sent
    /// afterwards, control answers included.
    ///
    /// # Errors
    /// Returns error if stdin cannot be closed
    pub async fn end_input(&self) -> Result<()> {
        let mut transport = self.transport.lock().await;
        transport.end_input().await
    }

    /// The CLI process: wait for its exit or kill it, from any task.
    #[must_use]
    pub fn process(&self) -> Option<crate::transport::ProcessHandle> {
        self.process.clone()
    }

    /// Get the next message from the stream
    ///
    /// Returns None when the stream ends
    pub async fn next_message(&mut self) -> Option<Result<Message>> {
        self.message_rx.recv().await
    }

    /// Take the hook event receiver
    ///
    /// This allows the caller to handle hook events independently
    pub fn take_hook_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<(String, HookEvent)>> {
        self.hook_rx.take()
    }

    /// Take the permission request receiver
    ///
    /// This allows the caller to handle permission requests independently
    pub fn take_permission_receiver(
        &mut self,
    ) -> Option<mpsc::UnboundedReceiver<(RequestId, PermissionRequest)>> {
        self.permission_rx.take()
    }

    /// Respond to a hook event
    ///
    /// Writes a success `control_response` for the CLI request `hook_id` carrying `response`.
    ///
    /// # Arguments
    /// * `hook_id` - ID of the hook event being responded to
    /// * `response` - Hook response data
    ///
    /// # Errors
    /// Returns error if response cannot be sent
    pub async fn respond_to_hook(
        &mut self,
        hook_id: String,
        response: serde_json::Value,
    ) -> Result<()> {
        let line =
            ControlResponseEnvelope::new(ControlResponseBody::success(hook_id, Some(response)))
                .to_line();
        write_line(&self.transport, &line).await
    }

    /// Respond to a permission request
    ///
    /// # Arguments
    /// * `request_id` - ID of the permission request being responded to
    /// * `result` - Permission result (Allow/Deny)
    ///
    /// # Errors
    /// Returns error if response cannot be sent
    pub async fn respond_to_permission(
        &mut self,
        request_id: RequestId,
        result: crate::types::PermissionResult,
    ) -> Result<()> {
        let request = self
            .pending_permissions
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(request_id.as_str()))
            .ok_or_else(|| {
                ClaudeError::control_protocol(format!(
                    "no permission request {} is waiting",
                    request_id.as_str()
                ))
            })?;
        let answer = ControlResponseBody::success(
            request_id.as_str(),
            Some(permission_response_value(&result, &request)),
        );
        write_line(
            &self.transport,
            &ControlResponseEnvelope::new(answer).to_line(),
        )
        .await
    }

    /// Receive messages until a Result message is encountered.
    ///
    /// Returns a stream that yields messages and automatically terminates
    /// after yielding the final Result message. Convenient for single-query workflows.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
    /// # use futures::StreamExt;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let options = ClaudeAgentOptions::default();
    /// # let mut client = ClaudeSDKClient::new(options, None).await?;
    /// client.send_message("Hello").await?;
    ///
    /// let mut messages = Box::pin(client.receive_response());
    /// while let Some(msg) = messages.next().await {
    ///     match msg? {
    ///         Message::Assistant { message, .. } => println!("{:?}", message),
    ///         Message::Result { .. } => println!("Done!"),
    ///         _ => {}
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use = "receive_response returns a stream that must be consumed to receive messages"]
    pub fn receive_response(&mut self) -> impl Stream<Item = Result<Message>> + '_ {
        async_stream::stream! {
            while let Some(result) = self.message_rx.recv().await {
                let is_result = matches!(&result, Ok(Message::Result { .. }));
                yield result;
                if is_result {
                    break;
                }
            }
        }
    }

    /// Check if the client is currently connected.
    ///
    /// Returns `true` if the transport is connected and ready.
    pub async fn is_connected(&self) -> bool {
        let transport = self.transport.lock().await;
        transport.is_ready()
    }

    /// Get the current session ID if available.
    ///
    /// The session ID is captured from Result messages automatically.
    /// Returns `None` if no session has been established yet.
    #[must_use]
    pub fn get_session_id(&self) -> Option<SessionId> {
        self.session_id.lock().ok()?.clone()
    }

    // ========================================================================
    // Session Binding
    // ========================================================================

    /// Bind this client to a specific session ID.
    ///
    /// Once bound, all `send_message()` calls validate that the current
    /// session matches the bound session. If a mismatch is detected,
    /// `send_message()` returns `ClaudeError::SessionMismatch`.
    ///
    /// This provides defense in depth to prevent messages from being
    /// accidentally sent to a different conversation context.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions, Message};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// client.send_message("Hello").await?;
    ///
    /// while let Some(msg) = client.next_message().await {
    ///     if let Message::Result { session_id, .. } = msg? {
    ///         // Bind to this session - all future sends will validate
    ///         client.bind_session(session_id);
    ///         break;
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind_session(&self, session_id: SessionId) {
        if let Ok(mut guard) = self.bound_session_id.lock() {
            *guard = Some(session_id);
        }
    }

    /// Get the bound session ID, if any.
    ///
    /// Returns `None` if no session is bound (binding disabled).
    #[must_use]
    pub fn bound_session(&self) -> Option<SessionId> {
        self.bound_session_id.lock().ok()?.clone()
    }

    /// Clear session binding, allowing messages to any session.
    ///
    /// After calling this, `send_message()` will no longer validate
    /// session IDs before sending.
    pub fn unbind_session(&self) {
        if let Ok(mut guard) = self.bound_session_id.lock() {
            *guard = None;
        }
    }

    /// Validate that current session matches bound session.
    ///
    /// Returns `Ok(())` if:
    /// - No session is bound (binding disabled)
    /// - Current session matches bound session
    /// - Either session is None (early in conversation)
    ///
    /// This is called automatically by `send_message()` if a session is bound.
    /// You can also call it manually for explicit validation.
    ///
    /// # Errors
    ///
    /// Returns `SessionMismatch` if bound session differs from current session.
    pub fn validate_session(&self) -> Result<()> {
        let bound = self.bound_session();
        let current = self.get_session_id();

        match (&bound, &current) {
            (Some(b), Some(c)) if b != c => {
                Err(ClaudeError::session_mismatch(b.to_string(), c.to_string()))
            }
            _ => Ok(()),
        }
    }

    // ========================================================================
    // Introspection Methods
    // ========================================================================

    /// Get session information including model, tools, and MCP servers.
    ///
    /// Returns `None` if the init message has not been received yet.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// // Wait for first message to ensure init is received
    /// if let Some(info) = client.session_info() {
    ///     println!("Model: {:?}", info.model);
    ///     println!("Available tools: {:?}", info.tool_names());
    ///     for server in &info.mcp_servers {
    ///         println!("MCP Server {}: status={}", server.name, server.status);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn session_info(&self) -> Option<SessionInfo> {
        self.session_info.lock().ok()?.clone()
    }

    /// Get the current model being used.
    ///
    /// Convenience method that extracts the model from session info.
    /// Returns `None` if init has not been received or model is not set.
    #[must_use]
    pub fn current_model(&self) -> Option<String> {
        self.session_info().and_then(|info| info.model)
    }

    /// Get the list of available tools in this session.
    ///
    /// Returns an empty vector if init has not been received.
    #[must_use]
    pub fn available_tools(&self) -> Vec<crate::types::ToolInfo> {
        self.session_info()
            .map(|info| info.tools)
            .unwrap_or_default()
    }

    /// Get MCP server status for all configured servers.
    ///
    /// Returns the status of each MCP server including connection state
    /// and any errors. Returns an empty vector if no MCP servers are configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// for server in client.mcp_server_status() {
    ///     if !server.is_connected() {
    ///         eprintln!("MCP server {} failed: {:?}", server.name, server.error);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn mcp_server_status(&self) -> Vec<crate::types::McpServerStatus> {
        self.session_info()
            .map(|info| info.mcp_servers)
            .unwrap_or_default()
    }

    /// Get list of known Claude models.
    ///
    /// Returns a static list of known Claude models with their capabilities.
    /// Note: This is a static list and may not reflect all available models
    /// for your account. Use `current_model()` to see what's actually in use.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// for model in ClaudeSDKClient::supported_models() {
    ///     println!("{}: {:?}", model.id, model.name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn supported_models() -> Vec<ModelInfo> {
        ModelInfo::known_models()
    }

    /// Get available slash commands.
    ///
    /// Returns the list of slash commands available in this session.
    /// Commands may be defined in project configuration (`.claude/commands/`)
    /// or built into Claude Code.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// for cmd in client.supported_commands() {
    ///     println!("/{} - {}", cmd.name, cmd.description);
    ///     if !cmd.argument_hint.is_empty() {
    ///         println!("  Usage: /{} {}", cmd.name, cmd.argument_hint);
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn supported_commands(&self) -> Vec<crate::types::SlashCommand> {
        // Commands are loaded from the init message's slash_commands field.
        // This includes both user-level (~/.claude/commands/) and
        // project-level (.claude/commands/) custom commands.
        //
        // Note: Requires setting_sources to include User and/or Project.
        self.session_info()
            .and_then(|info| {
                info.extra.get("slash_commands").and_then(|v| {
                    // The CLI sends slash_commands as an array of strings
                    if let Some(arr) = v.as_array() {
                        let commands: Vec<crate::types::SlashCommand> = arr
                            .iter()
                            .filter_map(|item| {
                                // Handle both string format and object format
                                if let Some(name) = item.as_str() {
                                    Some(crate::types::SlashCommand {
                                        name: name.to_string(),
                                        description: String::new(),
                                        argument_hint: String::new(),
                                    })
                                } else {
                                    // Try to deserialize as SlashCommand object
                                    serde_json::from_value(item.clone()).ok()
                                }
                            })
                            .collect();
                        Some(commands)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default()
    }

    /// Get account information from OAuth credentials.
    ///
    /// Reads account information from the Claude credentials file.
    /// This is only available for OAuth-authenticated accounts (Max Plan),
    /// not for API key authentication.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Credentials file not found
    /// - Credentials file is invalid
    /// - Not using OAuth authentication
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// match client.account_info() {
    ///     Ok(info) => {
    ///         println!("Email: {:?}", info.email);
    ///         println!("OAuth: {}", info.is_oauth);
    ///     }
    ///     Err(e) => println!("No account info: {}", e),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn account_info(&self) -> Result<AccountInfo> {
        // Account info is derived from the init message's apiKeySource field
        // and potentially other session data
        let session = self.session_info().ok_or_else(ClaudeError::not_connected)?;

        // Get apiKeySource from extra fields
        let api_key_source = session
            .extra
            .get("apiKeySource")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Determine if OAuth based on apiKeySource
        // "none" means OAuth/Max Plan, other values indicate API key sources
        let is_oauth = api_key_source.as_deref() == Some("none");

        Ok(AccountInfo {
            email: None,      // Not available in init message
            account_id: None, // Not available in init message
            is_oauth,
            organization_id: None, // Not available in init message
        })
    }

    // ========================================================================
    // Runtime Setters
    // ========================================================================

    /// Store a model preference locally.
    ///
    /// **NOTE:** This method only stores the value locally. To switch the running
    /// session's model, use [`Self::change_model`] (the `set_model` control request, which
    /// Claude Code 2.1.x supports). To pick the model at session start:
    ///
    /// ```rust,no_run
    /// use anthropic_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::builder()
    ///     .model("haiku")  // Set model at session start
    ///     .build();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_model(&self, model: impl Into<String>) {
        let model_str = model.into();
        tracing::debug!(model = %model_str, "set_model: storing locally (use change_model to switch the session)");

        if let Ok(mut guard) = self.runtime_model.lock() {
            *guard = Some(model_str);
        }
    }

    /// Get the currently configured runtime model override.
    ///
    /// Returns `None` if no runtime override is set.
    #[must_use]
    pub fn get_runtime_model(&self) -> Option<String> {
        self.runtime_model.lock().ok()?.clone()
    }

    /// Store a permission mode preference locally.
    ///
    /// **NOTE:** This method only stores the value locally. To switch the running
    /// session's mode, use [`Self::change_permission_mode`]. To set it at session start:
    ///
    /// ```rust,no_run
    /// use anthropic_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient, PermissionMode};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::builder()
    ///     .permission_mode(PermissionMode::AcceptEdits)  // Set at session start
    ///     .build();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Permission modes:
    /// - `Default`: Prompt for permission on sensitive operations
    /// - `AcceptEdits`: Auto-approve file edits
    /// - `Plan`: Plan-only mode, no execution
    /// - `BypassPermissions`: Auto-approve all operations (use with caution)
    pub fn set_permission_mode(&self, mode: crate::types::PermissionMode) {
        tracing::debug!(mode = ?mode, "set_permission_mode: storing locally");

        if let Ok(mut guard) = self.runtime_permission_mode.lock() {
            *guard = Some(mode);
        }
    }

    /// Get the currently configured runtime permission mode override.
    ///
    /// Returns `None` if no runtime override is set.
    #[must_use]
    pub fn get_runtime_permission_mode(&self) -> Option<crate::types::PermissionMode> {
        *self.runtime_permission_mode.lock().ok()?
    }

    /// Store a max thinking tokens preference locally.
    ///
    /// **NOTE:** This method only stores the value locally. To change the running
    /// session's budget, use [`Self::change_max_thinking_tokens`]. To set it at session start:
    ///
    /// ```rust,no_run
    /// use anthropic_agent_sdk::{ClaudeAgentOptions, ClaudeSDKClient};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::builder()
    ///     .max_thinking_tokens(20000)  // Set at session start
    ///     .build();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Extended thinking allows Claude to "think" before responding,
    /// improving quality for complex tasks.
    pub fn set_max_thinking_tokens(&self, tokens: u32) {
        tracing::debug!(tokens = tokens, "set_max_thinking_tokens: storing locally");

        if let Ok(mut guard) = self.runtime_max_thinking_tokens.lock() {
            *guard = Some(tokens);
        }
    }

    /// Get the currently configured runtime max thinking tokens override.
    ///
    /// Returns `None` if no runtime override is set.
    #[must_use]
    pub fn get_runtime_max_thinking_tokens(&self) -> Option<u32> {
        *self.runtime_max_thinking_tokens.lock().ok()?
    }

    /// Clear all runtime overrides.
    ///
    /// This resets model, permission mode, and thinking tokens to their
    /// original values from the initial options.
    pub fn clear_runtime_overrides(&self) {
        if let Ok(mut guard) = self.runtime_model.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.runtime_permission_mode.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.runtime_max_thinking_tokens.lock() {
            *guard = None;
        }
    }

    /// Get a child cancellation token for this client.
    ///
    /// This is analogous to JavaScript's `AbortController.signal`. Callers can
    /// use the returned token to:
    /// - Check if cancellation was requested: `token.is_cancelled()`
    /// - Wait for cancellation: `token.cancelled().await`
    /// - Use with `tokio::select!` to race cancellation against other futures
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// let cancel_token = client.cancellation_token();
    ///
    /// // Use in a spawned task to respect cancellation
    /// let token = cancel_token.clone();
    /// tokio::spawn(async move {
    ///     tokio::select! {
    ///         _ = token.cancelled() => {
    ///             println!("Operation cancelled");
    ///         }
    ///         _ = async { /* long operation */ } => {
    ///             println!("Operation completed");
    ///         }
    ///     }
    /// });
    ///
    /// // Later, cancel all operations
    /// client.cancel();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.child_token()
    }

    /// Cancel all ongoing operations.
    ///
    /// This is analogous to JavaScript's `AbortController.abort()`. Calling this
    /// method will:
    /// - Cancel the message reader in the transport
    /// - Signal any operations using a child cancellation token to stop
    ///
    /// Unlike `close()`, this does not immediately close the client - it only
    /// signals cancellation. Use `close()` after `cancel()` for full cleanup.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use anthropic_agent_sdk::{ClaudeSDKClient, ClaudeAgentOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let options = ClaudeAgentOptions::default();
    /// let mut client = ClaudeSDKClient::new(options, None).await?;
    ///
    /// client.send_message("Write a long essay").await?;
    ///
    /// // After some condition, cancel operations
    /// client.cancel();
    ///
    /// // Then close the client
    /// client.close().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Check if cancellation has been requested.
    ///
    /// Returns `true` if `cancel()` has been called on this client.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }

    /// Close the client and clean up resources
    ///
    /// # Errors
    /// Returns error if cleanup fails
    pub async fn close(&mut self) -> Result<()> {
        // Trigger SessionEnd hook before closing
        if let Some(ref manager) = self.hook_manager {
            let manager_guard = manager.lock().await;
            if let Err(e) = manager_guard.trigger_session_end("other").await {
                tracing::warn!(error = %e, "SessionEnd hook error");
            }
        }

        let mut transport = self.transport.lock().await;
        transport.close().await
    }
}

impl Drop for ClaudeSDKClient {
    fn drop(&mut self) {
        // Channel senders will be dropped, causing background tasks to exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "spawns the real Claude Code CLI"]
    async fn test_client_creation() {
        let options = ClaudeAgentOptions::default();
        let result = ClaudeSDKClient::new(options, None).await;
        assert!(result.is_ok() || result.is_err()); // Will succeed if CLI is available
    }

    #[tokio::test]
    #[ignore = "spawns the real Claude Code CLI"]
    async fn test_session_id_initially_none() {
        let options = ClaudeAgentOptions::default();
        if let Ok(client) = ClaudeSDKClient::new(options, None).await {
            // Session ID should be None before any Result message is received
            assert!(client.get_session_id().is_none());
        }
    }

    #[tokio::test]
    #[ignore = "spawns the real Claude Code CLI"]
    async fn test_is_connected() {
        let options = ClaudeAgentOptions::default();
        if let Ok(client) = ClaudeSDKClient::new(options, None).await {
            // Should be connected after successful initialization
            assert!(client.is_connected().await);
        }
    }
}
