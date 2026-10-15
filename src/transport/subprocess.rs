//! Subprocess transport implementation using Claude Code CLI

use async_trait::async_trait;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

use super::lines::{self, Line};
use super::process::{DirectSpawner, ProcessHandle, ProcessSpawner, SpawnRequest};
use crate::error::{ClaudeError, Result};
use crate::types::options::env_name_eq;
use crate::types::{ClaudeAgentOptions, ClaudeAuth, SystemPrompt, ToolName};
use crate::utils::truncate_for_display;
use crate::{Transport, VERSION};

const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024; // 1MB
/// How long the message stream waits, once the process has exited, for its stderr to end.
const STDERR_DRAIN: std::time::Duration = std::time::Duration::from_secs(2);

// Dangerous environment variables that should not be passed to subprocess
const DANGEROUS_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "PATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PERL5LIB",
    "RUBYLIB",
];

// Allowed extra CLI flags (allowlist approach)
const ALLOWED_EXTRA_FLAGS: &[&str] = &["timeout", "retries", "log-level", "cache-dir"];

/// An argument list under construction.
#[derive(Default)]
struct Args(Vec<OsString>);

impl Args {
    fn flag(&mut self, flag: impl Into<OsString>) {
        self.0.push(flag.into());
    }

    fn pair(&mut self, flag: impl Into<OsString>, value: impl Into<OsString>) {
        self.0.push(flag.into());
        self.0.push(value.into());
    }
}

/// Tool names joined for `--allowedTools` / `--disallowedTools`.
fn join_tools(tools: &[ToolName]) -> String {
    tools
        .iter()
        .map(ToolName::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

/// Prompt input type
#[derive(Debug)]
pub enum PromptInput {
    /// Single string prompt
    String(String),
    /// Stream of JSON messages
    Stream,
}

impl From<String> for PromptInput {
    fn from(s: String) -> Self {
        PromptInput::String(s)
    }
}

impl From<&str> for PromptInput {
    fn from(s: &str) -> Self {
        PromptInput::String(s.to_string())
    }
}

/// Subprocess transport for Claude Code CLI
pub struct SubprocessTransport {
    prompt: PromptInput,
    options: ClaudeAgentOptions,
    cli_path: PathBuf,
    cwd: Option<PathBuf>,
    process: Option<ProcessHandle>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    ready: Arc<AtomicBool>,
    max_buffer_size: usize,
    reader_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    /// Aborts the stderr task, also once the message reader holds it
    stderr_abort: Option<AbortHandle>,
    /// Cancellation token for aborting operations (like `AbortController` in JS)
    cancellation_token: CancellationToken,
}

impl SubprocessTransport {
    /// Create a new subprocess transport
    ///
    /// # Arguments
    /// * `prompt` - The prompt input (string or stream)
    /// * `options` - Configuration options
    /// * `cli_path` - Optional path to Claude Code CLI (will search if None)
    ///
    /// # Errors
    /// Returns error if CLI cannot be found
    pub fn new(
        prompt: PromptInput,
        options: ClaudeAgentOptions,
        cli_path: Option<PathBuf>,
    ) -> Result<Self> {
        Self::with_cancellation_token(prompt, options, cli_path, None)
    }

    /// Create a new subprocess transport with an optional parent cancellation token
    ///
    /// # Arguments
    /// * `prompt` - The prompt input (string or stream)
    /// * `options` - Configuration options
    /// * `cli_path` - Optional path to Claude Code CLI (will search if None)
    /// * `cancellation_token` - Optional parent cancellation token from client
    ///
    /// # Errors
    /// Returns error if CLI cannot be found
    pub fn with_cancellation_token(
        prompt: PromptInput,
        options: ClaudeAgentOptions,
        cli_path: Option<PathBuf>,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<Self> {
        tracing::debug!(
            "SubprocessTransport::new - output_format: {:?}",
            options.output_format.as_ref().map(|f| &f.format_type)
        );

        // An explicit path wins, then `path_to_claude_code_executable`, then a search.
        let cli_path = match cli_path.or_else(|| options.path_to_claude_code_executable.clone()) {
            Some(path) => path,
            None => Self::find_cli()?,
        };

        let cwd = options.cwd.clone();
        let max_buffer_size = options.max_buffer_size.unwrap_or(DEFAULT_MAX_BUFFER_SIZE);

        // Use provided token or create a new one
        let token = cancellation_token.unwrap_or_default();

        Ok(Self {
            prompt,
            options,
            cli_path,
            cwd,
            process: None,
            stdin: None,
            stdout: None,
            ready: Arc::new(AtomicBool::new(false)),
            max_buffer_size,
            reader_task: None,
            stderr_task: None,
            stderr_abort: None,
            cancellation_token: token,
        })
    }

    /// Find Claude Code CLI binary
    fn find_cli() -> Result<PathBuf> {
        // Try using 'which' crate first
        if let Ok(path) = which::which("claude") {
            return Ok(path);
        }

        // Manual search in common locations
        let home = env::var("HOME").unwrap_or_else(|_| String::from("/root"));
        let locations = vec![
            PathBuf::from(home.clone()).join(".npm-global/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            PathBuf::from(home.clone()).join(".local/bin/claude"),
            PathBuf::from(home.clone()).join("node_modules/.bin/claude"),
            PathBuf::from(home).join(".yarn/bin/claude"),
        ];

        for path in locations {
            if path.exists() && path.is_file() {
                return Ok(path);
            }
        }

        Err(ClaudeError::cli_not_found())
    }

    /// The CLI arguments these options and this prompt produce, in order.
    ///
    /// # Errors
    /// Returns `InvalidConfig` for contradictory options (a model context without a model,
    /// `resume_session_at` without `resume`, `sandbox` together with `settings`, `can_use_tool`
    /// together with `permission_prompt_tool_name`) or for extra flags outside the allowlist.
    #[allow(clippy::too_many_lines)]
    pub fn command_args(&self) -> Result<Vec<OsString>> {
        let options = &self.options;
        let mut args = Args::default();

        // Always use --print for non-interactive mode to avoid terminal manipulation
        args.flag("--print");
        args.pair("--output-format", "stream-json");
        args.flag("--verbose");

        // System prompt
        if let Some(ref system_prompt) = options.system_prompt {
            match system_prompt {
                SystemPrompt::String(s) => args.pair("--system-prompt", s),
                SystemPrompt::Preset(preset) => {
                    if let Some(ref append) = preset.append {
                        args.pair("--append-system-prompt", append);
                    }
                }
                SystemPrompt::File(path) => args.pair("--system-prompt-file", path),
            }
        }
        if let Some(ref append) = options.append_system_prompt {
            args.pair("--append-system-prompt", append);
        }

        if !options.allowed_tools.is_empty() {
            args.pair("--allowedTools", join_tools(&options.allowed_tools));
        }
        if let Some(max_turns) = options.max_turns {
            args.pair("--max-turns", max_turns.to_string());
        }
        if !options.disallowed_tools.is_empty() {
            args.pair("--disallowedTools", join_tools(&options.disallowed_tools));
        }

        // Model, with an optional context-window variant: `opus` + `1m` -> `opus[1m]`
        match (&options.model, &options.model_context) {
            (Some(model), Some(context)) => args.pair("--model", format!("{model}[{context}]")),
            (Some(model), None) => args.pair("--model", model),
            (None, Some(context)) => {
                return Err(ClaudeError::invalid_config(format!(
                    "model_context ({context}) needs a model"
                )));
            }
            (None, None) => {}
        }
        if let Some(effort) = options.effort {
            args.pair("--effort", effort.as_str());
        }
        if let Some(thinking) = options.thinking {
            args.pair("--thinking", thinking.as_str());
        }

        // Permissions. A `can_use_tool` callback is answered over stdin (`stdio`), which needs
        // streaming input.
        let stdio_permissions =
            options.can_use_tool.is_some() && matches!(self.prompt, PromptInput::Stream);
        match (&options.permission_prompt_tool_name, stdio_permissions) {
            (Some(_), true) => {
                return Err(ClaudeError::invalid_config(
                    "can_use_tool and permission_prompt_tool_name both answer permission prompts; set one",
                ));
            }
            (Some(tool), false) => args.pair("--permission-prompt-tool", tool),
            (None, true) => args.pair("--permission-prompt-tool", "stdio"),
            (None, false) => {}
        }
        if let Some(prompts) = options.permission_prompts {
            args.pair("--permission-prompts", prompts.as_str());
        }
        if let Some(mode) = options.permission_mode {
            args.pair("--permission-mode", mode.cli_value());
            // BypassPermissions requires the dangerous skip flag
            if mode == crate::types::PermissionMode::BypassPermissions {
                args.flag("--dangerously-skip-permissions");
            }
        }
        if options.allow_dangerously_skip_permissions {
            args.flag("--allow-dangerously-skip-permissions");
        }

        // Sessions
        if options.continue_conversation {
            args.flag("--continue");
        }
        if let Some(ref session_id) = options.resume {
            args.pair("--resume", session_id.as_str());
        }
        if let Some(ref resume_at) = options.resume_session_at {
            if options.resume.is_none() {
                return Err(ClaudeError::invalid_config(
                    "resume_session_at needs resume (the CLI requires --resume)",
                ));
            }
            args.pair("--resume-session-at", resume_at);
        }
        if options.fork_session {
            args.flag("--fork-session");
        }
        if let Some(ref session_id) = options.session_id {
            args.pair("--session-id", session_id);
        }
        if options.no_session_persistence {
            args.flag("--no-session-persistence");
        }
        if let Some(ref name) = options.session_name {
            args.pair("--name", name);
        }

        // Settings; sandbox settings travel inside a --settings JSON object
        match (&options.settings, &options.sandbox) {
            (Some(_), Some(_)) => {
                return Err(ClaudeError::invalid_config(
                    "sandbox is passed as --settings JSON; put it in the settings file instead of setting both",
                ));
            }
            (Some(settings), None) => args.pair("--settings", settings),
            (None, Some(sandbox)) => {
                let settings = serde_json::json!({ "sandbox": sandbox });
                args.pair("--settings", settings.to_string());
            }
            (None, None) => {}
        }
        if let Some(ref sources) = options.setting_sources {
            let sources: Vec<&str> = sources
                .iter()
                .map(|s| match s {
                    crate::types::SettingSource::User => "user",
                    crate::types::SettingSource::Project => "project",
                    crate::types::SettingSource::Local => "local",
                })
                .collect();
            args.pair("--setting-sources", sources.join(","));
        } else {
            args.pair("--setting-sources", "");
        }

        for dir in &options.add_dirs {
            args.pair("--add-dir", dir);
        }

        // MCP servers
        match &options.mcp_servers {
            crate::types::McpServers::Dict(servers) => {
                if !servers.is_empty() {
                    let config_map: serde_json::Map<String, serde_json::Value> = servers
                        .iter()
                        .map(|(name, config)| (name.clone(), Self::serialize_mcp_config(config)))
                        .collect();
                    let config_json = serde_json::json!({ "mcpServers": config_map });
                    args.pair("--mcp-config", config_json.to_string());
                }
            }
            crate::types::McpServers::Path(path) => args.pair("--mcp-config", path),
            crate::types::McpServers::None => {}
        }
        if options.strict_mcp_config {
            args.flag("--strict-mcp-config");
        }

        // Output stream contents
        if options.include_partial_messages {
            args.flag("--include-partial-messages");
        }
        if options.forward_subagent_text {
            args.flag("--forward-subagent-text");
        }
        if options.include_hook_events {
            args.flag("--include-hook-events");
        }

        if let Some(ref agents) = options.agents {
            let agents_json = serde_json::to_string(agents).unwrap_or_default();
            args.pair("--agents", agents_json);
        }
        if let Some(ref agent) = options.agent {
            args.pair("--agent", agent);
        }

        // `user` has no CLI flag (2.1.292 rejects `--user`); it is not passed.
        if options.user.is_some() {
            tracing::debug!("ClaudeAgentOptions::user is not passed: the CLI has no --user flag");
        }

        if let Some(max_budget) = options.max_budget_usd {
            args.pair("--max-budget-usd", max_budget.to_string());
        }
        if let Some(max_thinking) = options.max_thinking_tokens {
            args.pair("--max-thinking-tokens", max_thinking.to_string());
        }
        if let Some(ref fallback) = options.fallback_model {
            args.pair("--fallback-model", fallback);
        }
        if let Some(ref output_format) = options.output_format {
            let schema_json = serde_json::to_string(&output_format.schema).unwrap_or_default();
            tracing::debug!("Adding --json-schema flag with schema: {}", schema_json);
            args.pair("--json-schema", schema_json);
        }

        if let Some(ref plugins) = options.plugins {
            for plugin in plugins {
                match plugin {
                    crate::types::SdkPluginConfig::Local { path } => {
                        args.pair("--plugin-dir", path);
                    }
                }
            }
        }

        if let Some(ref betas) = options.betas {
            if !betas.is_empty() {
                args.flag("--betas");
                for beta in betas {
                    args.flag(beta.as_str());
                }
            }
        }

        if options.bare {
            args.flag("--bare");
        }
        if options.restricted {
            args.flag("--restricted");
        }
        if options.exclude_dynamic_system_prompt_sections {
            args.flag("--exclude-dynamic-system-prompt-sections");
        }

        // Extra args - strict allowlist enforcement
        // Reject any flags not in the allowlist to prevent CLI injection
        let mut disallowed: Vec<&str> = options
            .extra_args
            .keys()
            .map(String::as_str)
            .filter(|flag| !ALLOWED_EXTRA_FLAGS.contains(flag))
            .collect();
        if !disallowed.is_empty() {
            disallowed.sort_unstable();
            let flags_str = disallowed.join(", ");
            tracing::warn!(
                flags = %flags_str,
                allowed = ?ALLOWED_EXTRA_FLAGS,
                "Rejected disallowed CLI flags in extra_args"
            );
            return Err(ClaudeError::invalid_config(format!(
                "Disallowed CLI flags in extra_args: [{flags_str}]. Allowed flags: {ALLOWED_EXTRA_FLAGS:?}"
            )));
        }
        let mut extra: Vec<_> = options.extra_args.iter().collect();
        extra.sort();
        for (flag, value) in extra {
            match value {
                Some(v) => args.pair(format!("--{flag}"), v),
                None => args.flag(format!("--{flag}")),
            }
        }

        // Prompt handling based on mode
        match &self.prompt {
            PromptInput::Stream => {
                // Streaming mode: stream-json on stdin; the CLI echoes each user message
                // (with its uuid) once it takes it.
                args.pair("--input-format", "stream-json");
                args.flag("--replay-user-messages");
            }
            PromptInput::String(s) => {
                // String mode: pass the prompt as an argument after --
                args.flag("--");
                args.flag(s);
            }
        }

        Ok(args.0)
    }

    /// The environment of the CLI process, built from `parent` (the SDK's own environment).
    ///
    /// Starts from the parent variables `env_inheritance` allows, drops inherited credential
    /// variables unless `auth` inherits them, then adds `env`, the credential, `CLAUDE_CONFIG_DIR`,
    /// `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` and the SDK's entrypoint variables.
    ///
    /// # Errors
    /// Returns `InvalidConfig` when `env` sets a variable on the dangerous list.
    pub fn command_env(
        &self,
        parent: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Vec<(String, String)>> {
        let options = &self.options;

        // Check for dangerous env vars in user-provided options (strict enforcement)
        let mut dangerous_found: Vec<&str> = options
            .env
            .keys()
            .map(String::as_str)
            .filter(|key| DANGEROUS_ENV_VARS.iter().any(|d| env_name_eq(d, key)))
            .collect();
        if !dangerous_found.is_empty() {
            dangerous_found.sort_unstable();
            let vars_str = dangerous_found.join(", ");
            tracing::warn!(
                vars = %vars_str,
                "Rejected dangerous environment variables - possible injection attempt"
            );
            return Err(ClaudeError::invalid_config(format!(
                "Dangerous environment variables detected: [{vars_str}]. These are blocked to prevent injection attacks."
            )));
        }

        let credential = options.auth.env_var();
        let replaces_inherited = options.auth.replaces_inherited();
        let mut env: Vec<(String, String)> = parent
            .into_iter()
            .filter(|(name, _)| options.env_inheritance.allows(name))
            .filter(|(name, _)| {
                !replaces_inherited
                    || !ClaudeAuth::CREDENTIAL_VARS
                        .iter()
                        .any(|var| env_name_eq(var, name))
            })
            .collect();

        let mut set = |name: &str, value: String| {
            env.retain(|(existing, _)| !env_name_eq(existing, name));
            env.push((name.to_string(), value));
        };
        let mut user_env: Vec<_> = options.env.iter().collect();
        user_env.sort();
        for (key, value) in user_env {
            set(key, value.clone());
        }
        if let Some((name, value)) = credential {
            set(name, value.to_string());
        }
        if let Some(ref dir) = options.config_dir {
            set("CLAUDE_CONFIG_DIR", dir.to_string_lossy().into_owned());
        }
        if options.enable_file_checkpointing {
            set("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "1".to_string());
        }
        if let Some(ref cwd) = self.cwd {
            set("PWD", cwd.to_string_lossy().into_owned());
        }
        set("CLAUDE_CODE_ENTRYPOINT", "sdk-rust".to_string());
        set("CLAUDE_AGENT_SDK_VERSION", VERSION.to_string());
        Ok(env)
    }

    /// The CLI command: program, arguments, environment and working directory.
    ///
    /// # Errors
    /// Returns `InvalidConfig` for options [`Self::command_args`] or [`Self::command_env`]
    /// refuse.
    pub fn spawn_request(&self) -> Result<SpawnRequest> {
        Ok(SpawnRequest {
            program: self.cli_path.clone(),
            args: self.command_args()?,
            env: self.command_env(env::vars())?,
            cwd: self.cwd.clone(),
        })
    }

    /// The running process, once connected.
    #[must_use]
    pub fn process(&self) -> Option<ProcessHandle> {
        self.process.clone()
    }

    /// Get a child cancellation token for this transport
    /// Callers can use this to cancel ongoing operations
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.child_token()
    }

    /// Cancel all ongoing operations
    pub fn cancel(&self) {
        self.cancellation_token.cancel();
    }

    /// Serialize MCP config for CLI
    fn serialize_mcp_config(config: &crate::types::McpServerConfig) -> serde_json::Value {
        match config {
            crate::types::McpServerConfig::Stdio(stdio) => {
                let mut obj = serde_json::json!({
                    "command": stdio.command,
                });
                if let Some(ref args) = stdio.args {
                    obj["args"] = serde_json::json!(args);
                }
                if let Some(ref env) = stdio.env {
                    obj["env"] = serde_json::json!(env);
                }
                if let Some(ref server_type) = stdio.server_type {
                    obj["type"] = serde_json::json!(server_type);
                }
                obj
            }
            crate::types::McpServerConfig::Sse(sse) => {
                serde_json::json!({
                    "type": sse.server_type,
                    "url": sse.url,
                    "headers": sse.headers,
                })
            }
            crate::types::McpServerConfig::Http(http) => {
                serde_json::json!({
                    "type": http.server_type,
                    "url": http.url,
                    "headers": http.headers,
                })
            }
            crate::types::McpServerConfig::Sdk(sdk) => {
                let mut obj = serde_json::json!({
                    "type": "sdk",
                    "name": sdk.name,
                });
                if let Some(ref version) = sdk.version {
                    obj["version"] = serde_json::json!(version);
                }
                obj
            }
        }
    }
}

#[async_trait]
impl Transport for SubprocessTransport {
    async fn connect(&mut self) -> Result<()> {
        if self.process.is_some() {
            return Ok(());
        }

        tracing::debug!(
            "SubprocessTransport::connect - output_format is {:?}",
            self.options.output_format.as_ref().map(|f| &f.format_type)
        );

        // Stdin, stdout and stderr are piped: an inherited stderr would let the child
        // manipulate the parent terminal's state.
        let request = self.spawn_request()?;

        // Log the program only: arguments and environment can carry secrets (MCP headers,
        // credentials).
        tracing::debug!(program = ?self.cli_path, "Starting Claude Code CLI");

        // Spawn process, through the caller's spawner when there is one
        let spawned = match &self.options.spawner {
            Some(spawner) => spawner.spawn(request),
            None => DirectSpawner.spawn(request),
        };
        let mut child = spawned.map_err(|e| {
            if let Some(ref cwd) = self.cwd {
                if !cwd.exists() {
                    #[cfg(debug_assertions)]
                    return ClaudeError::connection(format!(
                        "Working directory does not exist: {}",
                        cwd.display()
                    ));
                    #[cfg(not(debug_assertions))]
                    return ClaudeError::connection("Working directory does not exist".to_string());
                }
            }
            ClaudeError::connection(format!("Failed to start Claude Code: {e}"))
        })?;

        // Get stdin, stdout, and stderr
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClaudeError::connection("Failed to get stdin handle"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClaudeError::connection("Failed to get stdout handle"))?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ClaudeError::connection("Failed to get stderr handle"))?;

        // Spawn task to consume stderr to prevent blocking. Each line goes to the `stderr`
        // callback when one is set, else to the parent's stderr.
        let stderr_callback = self.options.stderr.clone();
        let max_buffer_size = self.max_buffer_size;
        let stderr_task = tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            while let Ok(line) = lines::read_line(&mut stderr, max_buffer_size).await {
                let Some(line) = line.text() else { break };
                match stderr_callback {
                    Some(ref callback) => callback(line),
                    None => {
                        let _ = std::io::Write::write_all(
                            &mut std::io::stderr(),
                            format!(
                                "{line}
"
                            )
                            .as_bytes(),
                        );
                    }
                }
            }
        });

        // Store handles
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));
        self.process = Some(ProcessHandle::follow(child));
        self.stderr_abort = Some(stderr_task.abort_handle());
        self.stderr_task = Some(stderr_task);
        self.ready.store(true, Ordering::SeqCst);

        // For string mode, close stdin immediately
        if matches!(self.prompt, PromptInput::String(_)) {
            if let Some(mut stdin) = self.stdin.take() {
                let _ = stdin.shutdown().await;
            }
        }

        Ok(())
    }

    async fn write(&mut self, data: &str) -> Result<()> {
        if !self.is_ready() {
            return Err(ClaudeError::transport("Transport is not ready for writing"));
        }

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ClaudeError::transport("stdin not available"))?;

        stdin
            .write_all(data.as_bytes())
            .await
            .map_err(|e| ClaudeError::transport(format!("Failed to write to stdin: {e}")))?;

        stdin
            .flush()
            .await
            .map_err(|e| ClaudeError::transport(format!("Failed to flush stdin: {e}")))?;

        Ok(())
    }

    async fn end_input(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            stdin
                .shutdown()
                .await
                .map_err(|e| ClaudeError::transport(format!("Failed to close stdin: {e}")))?;
        }
        Ok(())
    }

    fn read_messages(&mut self) -> mpsc::UnboundedReceiver<Result<serde_json::Value>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Take ownership of stdout; the process stays shared
        let stdout = self.stdout.take();
        let process = self.process.clone();
        let stdout_callback = self.options.stdout.clone();
        let max_buffer_size = self.max_buffer_size;
        let cancel_token = self.cancellation_token.clone();
        let stderr_task = self.stderr_task.take();

        // Spawn background task to read messages
        let task = tokio::spawn(async move {
            if stdout.is_none() {
                let _ = tx.send(Err(ClaudeError::connection(
                    "Not connected - stdout not available",
                )));
                return;
            }

            let mut stdout = stdout.unwrap();

            loop {
                // Use select! to allow cancellation - no hardcoded timeout
                // The caller controls cancellation via the CancellationToken
                let line = tokio::select! {
                    () = cancel_token.cancelled() => {
                        tracing::debug!("Read cancelled via CancellationToken");
                        break;
                    }
                    line = lines::read_line(&mut stdout, max_buffer_size) => line,
                };
                let line = match line {
                    Ok(Line::End) => break,
                    Ok(line) => line,
                    Err(e) => {
                        let _ = tx.send(Err(ClaudeError::Io(e)));
                        break;
                    }
                };
                if let (Some(callback), Some(text)) = (&stdout_callback, line.text()) {
                    callback(text);
                }
                // Each line is one message: a malformed or oversized line is reported and
                // dropped, and never affects the lines after it.
                let parsed = match line {
                    Line::Whole(bytes) if bytes.trim_ascii().is_empty() => continue,
                    Line::Whole(bytes) => {
                        serde_json::from_slice::<serde_json::Value>(bytes.trim_ascii())
                            .map_err(ClaudeError::JsonDecode)
                    }
                    Line::Oversized { head, len } => {
                        let preview = truncate_for_display(&String::from_utf8_lossy(&head), 100);
                        Err(ClaudeError::JsonDecode(serde_json::Error::io(
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "JSON message of {len} bytes exceeded maximum buffer size of {max_buffer_size} bytes. Preview: {preview}"
                                ),
                            ),
                        )))
                    }
                    Line::End => break,
                };
                if let Ok(data) = &parsed {
                    let msg_type = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    tracing::trace!(msg_type, "Received message from CLI");
                }
                if tx.send(parsed).is_err() {
                    // Receiver dropped, stop reading
                    break;
                }
            }

            // Check process exit code
            if let Some(process) = process {
                if let Some(code) = process.wait().await.code.filter(|code| *code != 0) {
                    let _ = tx.send(Err(ClaudeError::process(
                        "Command failed",
                        code,
                        Some("Check stderr output for details".to_string()),
                    )));
                }
            }
            // The stream ends after the process's last stderr lines reached the callback; a
            // pipe some grandchild still holds open delays the end by `STDERR_DRAIN` at most.
            if let Some(task) = stderr_task {
                let _ = tokio::time::timeout(STDERR_DRAIN, task).await;
            }
        });

        // Store task handle for cleanup
        self.reader_task = Some(task);

        rx
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    async fn close(&mut self) -> Result<()> {
        self.ready.store(false, Ordering::SeqCst);

        // Cancel any ongoing read operations via token
        self.cancellation_token.cancel();

        // Close stdin to signal the process to exit gracefully
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
        }

        // Wait for reader task to finish (it will exit due to cancellation)
        if let Some(task) = self.reader_task.take() {
            // Give a brief window for graceful exit before abort
            tokio::select! {
                _ = task => {}
                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
        if let Some(task) = self.stderr_abort.take() {
            task.abort();
        }
        self.stderr_task = None;

        self.stdout = None;

        // Try to wait for the process to exit gracefully first
        if let Some(process) = self.process.take() {
            // Give the process a configurable timeout to exit gracefully
            let timeout_duration = std::time::Duration::from_secs(5);
            if tokio::time::timeout(timeout_duration, process.wait())
                .await
                .is_err()
            {
                // Timeout - kill the process
                process.kill();
                process.wait().await;
            }
        }

        Ok(())
    }
}

impl Drop for SubprocessTransport {
    fn drop(&mut self) {
        // Close stdin if still open to signal graceful shutdown
        if let Some(stdin) = self.stdin.take() {
            // Drop will close it
            drop(stdin);
        }

        // Abort reader task if running
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }

        // The stderr task is left to read to the pipe's end, which comes with the process's:
        // the last lines a process writes before it exits still reach the stderr callback.
        drop(self.stderr_task.take());

        // Try graceful shutdown first, then kill if needed
        if let Some(process) = self.process.take() {
            if process.exited().is_none() {
                process.kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ClaudeAuth, Effort, EnvInheritance, PermissionMode, PermissionPrompts, SandboxSettings,
        SdkBeta, ThinkingMode,
    };

    /// A CLI path that is never started: these tests only build arguments and environments.
    fn fake_cli() -> Option<PathBuf> {
        Some(PathBuf::from("claude-not-started"))
    }

    fn transport(prompt: PromptInput, options: ClaudeAgentOptions) -> SubprocessTransport {
        SubprocessTransport::new(prompt, options, fake_cli()).expect("transport")
    }

    fn args(options: ClaudeAgentOptions) -> Vec<String> {
        args_for(PromptInput::Stream, options)
    }

    fn args_for(prompt: PromptInput, options: ClaudeAgentOptions) -> Vec<String> {
        transport(prompt, options)
            .command_args()
            .expect("valid options")
            .into_iter()
            .map(|a| a.into_string().expect("utf-8"))
            .collect()
    }

    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(String::as_str)
    }

    fn config_error(options: ClaudeAgentOptions) -> String {
        transport(PromptInput::Stream, options)
            .command_args()
            .expect_err("invalid options")
            .to_string()
    }

    #[test]
    fn test_find_cli() {
        // This will succeed if claude is installed
        let result = SubprocessTransport::find_cli();
        println!("CLI search result: {result:?}");
    }

    #[test]
    fn test_prompt_input_conversions() {
        let _prompt1: PromptInput = "hello".into();
        let _prompt2: PromptInput = String::from("world").into();
    }

    #[test]
    fn streaming_mode_uses_stream_json_both_ways_and_replays_user_messages() {
        let args = args(ClaudeAgentOptions::default());
        assert_eq!(
            &args[..4],
            ["--print", "--output-format", "stream-json", "--verbose"]
        );
        assert_eq!(value_of(&args, "--input-format"), Some("stream-json"));
        assert!(args.contains(&"--replay-user-messages".to_string()));
        assert_eq!(value_of(&args, "--setting-sources"), Some(""));
    }

    #[test]
    fn string_mode_passes_the_prompt_after_double_dash() {
        let args = args_for("hi".into(), ClaudeAgentOptions::default());
        assert_eq!(&args[args.len() - 2..], ["--", "hi"]);
        assert!(!args.contains(&"--input-format".to_string()));
    }

    #[test]
    fn model_effort_and_context_window() {
        let args = args(
            ClaudeAgentOptions::builder()
                .model("opus")
                .model_context("1m")
                .effort(Effort::XHigh)
                .thinking(ThinkingMode::Adaptive)
                .build(),
        );
        assert_eq!(value_of(&args, "--model"), Some("opus[1m]"));
        assert_eq!(value_of(&args, "--effort"), Some("xhigh"));
        assert_eq!(value_of(&args, "--thinking"), Some("adaptive"));

        for (effort, text) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            assert_eq!(effort.as_str(), text);
            assert_eq!(text.parse::<Effort>().unwrap(), effort);
        }
        assert!("ultra".parse::<Effort>().is_err());

        let plain = self::args(ClaudeAgentOptions::builder().model("haiku").build());
        assert_eq!(value_of(&plain, "--model"), Some("haiku"));
        assert!(
            config_error(ClaudeAgentOptions::builder().model_context("1m").build())
                .contains("needs a model")
        );
    }

    #[test]
    fn every_permission_mode_has_its_cli_value() {
        for (mode, text) in [
            (PermissionMode::Default, "default"),
            (PermissionMode::Manual, "manual"),
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::Plan, "plan"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::DontAsk, "dontAsk"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
        ] {
            let args = args(ClaudeAgentOptions::builder().permission_mode(mode).build());
            assert_eq!(value_of(&args, "--permission-mode"), Some(text));
            assert_eq!(
                args.contains(&"--dangerously-skip-permissions".to_string()),
                mode == PermissionMode::BypassPermissions
            );
            assert_eq!(PermissionMode::from_cli_value(text), Some(mode));
        }
        assert_eq!(PermissionMode::Manual.control_value(), "default");
    }

    #[test]
    fn permission_prompt_tool_prompts_and_tool_lists() {
        let args = args(
            ClaudeAgentOptions::builder()
                .permission_prompt_tool_name("mcp__tracker__permission_prompt")
                .permission_prompts(PermissionPrompts::Host)
                .allowed_tools(vec![ToolName::new("Read"), ToolName::new("Bash(git *)")])
                .disallowed_tools(vec![ToolName::new("AskUserQuestion")])
                .allow_dangerously_skip_permissions(true)
                .build(),
        );
        assert_eq!(
            value_of(&args, "--permission-prompt-tool"),
            Some("mcp__tracker__permission_prompt")
        );
        assert_eq!(value_of(&args, "--permission-prompts"), Some("host"));
        assert_eq!(value_of(&args, "--allowedTools"), Some("Read,Bash(git *)"));
        assert_eq!(
            value_of(&args, "--disallowedTools"),
            Some("AskUserQuestion")
        );
        assert!(args.contains(&"--allow-dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn can_use_tool_answers_over_stdio() {
        let callback = crate::PermissionManager::callback(|_, _, _| async {
            Ok(crate::types::PermissionResult::Allow(
                crate::types::PermissionResultAllow {
                    updated_input: None,
                    updated_permissions: None,
                },
            ))
        });
        let options = ClaudeAgentOptions::builder()
            .can_use_tool(callback.clone())
            .build();
        assert_eq!(
            value_of(&args(options), "--permission-prompt-tool"),
            Some("stdio")
        );
        let both = ClaudeAgentOptions::builder()
            .can_use_tool(callback)
            .permission_prompt_tool_name("mcp__x__y")
            .build();
        assert!(config_error(both).contains("set one"));
    }

    #[test]
    fn sessions_resume_continue_fork_and_persistence() {
        let args = args(
            ClaudeAgentOptions::builder()
                .resume("s1")
                .resume_session_at("u9")
                .fork_session(true)
                .continue_conversation(true)
                .no_session_persistence(true)
                .session_name("nightly")
                .build(),
        );
        assert_eq!(value_of(&args, "--resume"), Some("s1"));
        assert_eq!(value_of(&args, "--resume-session-at"), Some("u9"));
        assert!(!args.contains(&"--resume-at".to_string()));
        assert!(args.contains(&"--fork-session".to_string()));
        assert!(args.contains(&"--continue".to_string()));
        assert!(args.contains(&"--no-session-persistence".to_string()));
        assert_eq!(value_of(&args, "--name"), Some("nightly"));
        assert!(
            config_error(
                ClaudeAgentOptions::builder()
                    .resume_session_at("u9")
                    .build()
            )
            .contains("needs resume")
        );
    }

    #[test]
    fn test_session_id_option_maps_to_cli_arg() {
        let args = args(
            ClaudeAgentOptions::builder()
                .session_id("550e8400-e29b-41d4-a716-446655440000")
                .build(),
        );
        assert_eq!(
            value_of(&args, "--session-id"),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn mcp_config_strict_and_add_dirs() {
        let args = args(
            ClaudeAgentOptions::builder()
                .mcp_servers(crate::types::McpServers::Path(PathBuf::from("mcp.json")))
                .strict_mcp_config(true)
                .add_dirs(vec![PathBuf::from("a"), PathBuf::from("b")])
                .build(),
        );
        assert_eq!(value_of(&args, "--mcp-config"), Some("mcp.json"));
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        let dirs: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "--add-dir")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(dirs, ["a", "b"]);
    }

    #[test]
    fn stream_content_flags() {
        let args = args(
            ClaudeAgentOptions::builder()
                .include_partial_messages(true)
                .forward_subagent_text(true)
                .include_hook_events(true)
                .bare(true)
                .restricted(true)
                .exclude_dynamic_system_prompt_sections(true)
                .agent("reviewer")
                .build(),
        );
        for flag in [
            "--include-partial-messages",
            "--forward-subagent-text",
            "--include-hook-events",
            "--bare",
            "--restricted",
            "--exclude-dynamic-system-prompt-sections",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag} missing");
        }
        assert_eq!(value_of(&args, "--agent"), Some("reviewer"));
    }

    #[test]
    fn flags_the_cli_rejects_are_not_built() {
        // 2.1.292 rejects --user, --beta, --resume-at and every --sandbox* flag.
        let args = args(
            ClaudeAgentOptions::builder()
                .user("someone")
                .betas(vec![SdkBeta::Context1M])
                .sandbox(SandboxSettings {
                    enabled: Some(true),
                    auto_allow_bash_if_sandboxed: Some(true),
                    ..SandboxSettings::default()
                })
                .build(),
        );
        assert!(
            !args
                .iter()
                .any(|a| a == "--user" || a == "--beta" || a.starts_with("--sandbox"))
        );
        assert_eq!(value_of(&args, "--betas"), Some("context-1m-2025-08-07"));
        let settings: serde_json::Value =
            serde_json::from_str(value_of(&args, "--settings").unwrap()).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"sandbox": {"enabled": true, "autoAllowBashIfSandboxed": true}})
        );
        let both = ClaudeAgentOptions::builder()
            .settings("settings.json")
            .sandbox(SandboxSettings::default())
            .build();
        assert!(config_error(both).contains("sandbox"));
    }

    #[test]
    fn test_plugin_dir_flag_correct() {
        let args = args(
            ClaudeAgentOptions::builder()
                .plugins(vec![crate::types::SdkPluginConfig::Local {
                    path: "/path/to/plugin".to_string(),
                }])
                .build(),
        );
        assert_eq!(value_of(&args, "--plugin-dir"), Some("/path/to/plugin"));
    }

    #[test]
    fn test_extra_args_allowlist_rejects_disallowed() {
        let mut options = ClaudeAgentOptions::default();
        options
            .extra_args
            .insert("dangerous-flag".to_string(), None);
        let err = config_error(options);
        assert!(err.contains("Disallowed CLI flags"));
        assert!(err.contains("dangerous-flag"));
    }

    #[test]
    fn test_extra_args_allowlist_accepts_allowed() {
        let mut options = ClaudeAgentOptions::default();
        options
            .extra_args
            .insert("timeout".to_string(), Some("30".to_string()));
        options
            .extra_args
            .insert("log-level".to_string(), Some("debug".to_string()));
        let args = args(options);
        assert_eq!(value_of(&args, "--timeout"), Some("30"));
        assert_eq!(value_of(&args, "--log-level"), Some("debug"));
    }

    fn env_of(options: ClaudeAgentOptions, parent: &[(&str, &str)]) -> Vec<(String, String)> {
        transport(PromptInput::Stream, options)
            .command_env(
                parent
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
            )
            .expect("valid env")
    }

    fn get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    const PARENT: [(&str, &str); 6] = [
        ("PATH", "/bin"),
        ("HOME", "/home/u"),
        ("ANTHROPIC_API_KEY", "parent-key"),
        ("ANTHROPIC_AUTH_TOKEN", "parent-auth"),
        ("TRACKER_SECRET", "s3cret"),
        ("CLAUDE_CODE_USE_BEDROCK", "0"),
    ];

    #[test]
    fn env_inherits_everything_by_default() {
        let env = env_of(ClaudeAgentOptions::default(), &PARENT);
        assert_eq!(get(&env, "TRACKER_SECRET"), Some("s3cret"));
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), Some("parent-key"));
        assert_eq!(get(&env, "CLAUDE_CODE_ENTRYPOINT"), Some("sdk-rust"));
        assert_eq!(get(&env, "CLAUDE_AGENT_SDK_VERSION"), Some(crate::VERSION));
    }

    #[test]
    fn env_allow_list_with_prefixes() {
        let options = ClaudeAgentOptions::builder()
            .env_inheritance(EnvInheritance::Only(vec![
                "PATH".into(),
                "HOME".into(),
                "CLAUDE_*".into(),
            ]))
            .config_dir("/runs/r1/claude")
            .build();
        let env = env_of(options, &PARENT);
        assert_eq!(get(&env, "PATH"), Some("/bin"));
        assert_eq!(get(&env, "CLAUDE_CODE_USE_BEDROCK"), Some("0"));
        assert_eq!(get(&env, "TRACKER_SECRET"), None);
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), None);
        assert_eq!(get(&env, "CLAUDE_CONFIG_DIR"), Some("/runs/r1/claude"));
    }

    #[test]
    fn oauth_token_replaces_every_inherited_credential() {
        let options = ClaudeAgentOptions::builder()
            .auth(ClaudeAuth::OAuthToken("setup-token".into()))
            .build();
        let env = env_of(options, &PARENT);
        assert_eq!(get(&env, "CLAUDE_CODE_OAUTH_TOKEN"), Some("setup-token"));
        assert_eq!(get(&env, "ANTHROPIC_API_KEY"), None);
        assert_eq!(get(&env, "ANTHROPIC_AUTH_TOKEN"), None);
        assert_eq!(get(&env, "TRACKER_SECRET"), Some("s3cret"));

        let key = env_of(
            ClaudeAgentOptions::builder()
                .auth(ClaudeAuth::ApiKey("sk-test".into()))
                .build(),
            &[("CLAUDE_CODE_OAUTH_TOKEN", "old")],
        );
        assert_eq!(get(&key, "ANTHROPIC_API_KEY"), Some("sk-test"));
        assert_eq!(get(&key, "CLAUDE_CODE_OAUTH_TOKEN"), None);
    }

    #[test]
    fn a_login_drops_every_inherited_credential() {
        let options = ClaudeAgentOptions::builder()
            .auth(ClaudeAuth::Login)
            .config_dir("/team/claude")
            .build();
        let env = env_of(
            options,
            &[
                ("CLAUDE_CODE_OAUTH_TOKEN", "old"),
                ("PATH", "/bin"),
                ("ANTHROPIC_API_KEY", "parent-key"),
                ("ANTHROPIC_AUTH_TOKEN", "parent-auth"),
            ],
        );
        assert_eq!(get(&env, "CLAUDE_CONFIG_DIR"), Some("/team/claude"));
        assert_eq!(get(&env, "PATH"), Some("/bin"));
        for var in ClaudeAuth::CREDENTIAL_VARS {
            assert_eq!(get(&env, var), None, "{var}");
        }
    }

    #[test]
    fn credentials_never_reach_debug_output() {
        let options = ClaudeAgentOptions::builder()
            .auth(ClaudeAuth::OAuthToken("very-secret".into()))
            .env([("X_TOKEN".to_string(), "env-secret".to_string())].into())
            .build();
        let text = format!("{options:?}");
        assert!(!text.contains("very-secret"));
        assert!(!text.contains("env-secret"));
        assert!(text.contains("OAuthToken(<redacted>)"));
    }

    #[test]
    fn test_dangerous_env_vars_rejected() {
        let mut options = ClaudeAgentOptions::default();
        options
            .env
            .insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        let err = transport(PromptInput::Stream, options)
            .command_env(Vec::new())
            .expect_err("rejected")
            .to_string();
        assert!(err.contains("Dangerous environment variables"));
        assert!(err.contains("LD_PRELOAD"));
    }

    #[test]
    fn test_safe_env_vars_accepted() {
        let mut options = ClaudeAgentOptions::default();
        options
            .env
            .insert("MY_SAFE_VAR".to_string(), "safe value".to_string());
        let env = env_of(options, &[("MY_SAFE_VAR", "parent")]);
        assert_eq!(get(&env, "MY_SAFE_VAR"), Some("safe value"));
        assert_eq!(env.iter().filter(|(k, _)| k == "MY_SAFE_VAR").count(), 1);
    }

    #[test]
    fn file_checkpointing_is_an_env_var() {
        let env = env_of(
            ClaudeAgentOptions::builder()
                .enable_file_checkpointing(true)
                .build(),
            &[],
        );
        assert_eq!(
            get(&env, "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"),
            Some("1")
        );
    }
}
