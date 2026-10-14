//! Claude Agent configuration options

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use typed_builder::TypedBuilder;

use super::hooks::{HookEvent, HookMatcher};
use super::identifiers::ToolName;
use super::mcp::McpServers;
use super::permissions::{CanUseToolCallback, PermissionMode, SettingSource};

// ============================================================================
// System Prompt Types
// ============================================================================

/// System prompt preset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptPreset {
    /// Prompt type (always "preset")
    #[serde(rename = "type")]
    pub prompt_type: String,
    /// Preset name (e.g., "`claude_code`")
    pub preset: String,
    /// Additional text to append to the preset
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
}

/// System prompt configuration
#[derive(Debug, Clone)]
pub enum SystemPrompt {
    /// Plain string system prompt
    String(String),
    /// Preset-based system prompt
    Preset(SystemPromptPreset),
    /// Load system prompt from a file (uses `--system-prompt-file` CLI flag)
    File(PathBuf),
}

// Implement conversions for SystemPrompt
impl From<String> for SystemPrompt {
    fn from(s: String) -> Self {
        SystemPrompt::String(s)
    }
}

impl From<&str> for SystemPrompt {
    fn from(s: &str) -> Self {
        SystemPrompt::String(s.to_string())
    }
}

impl From<SystemPromptPreset> for SystemPrompt {
    fn from(preset: SystemPromptPreset) -> Self {
        SystemPrompt::Preset(preset)
    }
}

impl From<PathBuf> for SystemPrompt {
    fn from(path: PathBuf) -> Self {
        SystemPrompt::File(path)
    }
}

impl SystemPrompt {
    /// Create a file-based system prompt
    #[must_use]
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        SystemPrompt::File(path.into())
    }
}

// ============================================================================
// Tools Configuration
// ============================================================================

/// Tools configuration - either a list of tool names or a preset
#[derive(Debug, Clone)]
pub enum ToolsConfig {
    /// Explicit list of tool names
    List(Vec<ToolName>),
    /// Use a preset (e.g., `claude_code`)
    Preset(ToolsPreset),
}

/// Tools preset configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsPreset {
    /// Type (always "preset")
    #[serde(rename = "type")]
    pub config_type: String,
    /// Preset name
    pub preset: String,
}

impl ToolsPreset {
    /// Create the Claude Code tools preset
    #[must_use]
    pub fn claude_code() -> Self {
        Self {
            config_type: "preset".to_string(),
            preset: "claude_code".to_string(),
        }
    }
}

impl ToolsConfig {
    /// Create a Claude Code preset
    #[must_use]
    pub fn claude_code_preset() -> Self {
        Self::Preset(ToolsPreset::claude_code())
    }

    /// Create from a list of tool names
    #[must_use]
    pub fn from_list(tools: Vec<ToolName>) -> Self {
        Self::List(tools)
    }
}

impl From<Vec<ToolName>> for ToolsConfig {
    fn from(tools: Vec<ToolName>) -> Self {
        Self::List(tools)
    }
}

impl From<ToolsPreset> for ToolsConfig {
    fn from(preset: ToolsPreset) -> Self {
        Self::Preset(preset)
    }
}

// ============================================================================
// Stderr Callback
// ============================================================================

use std::sync::Arc;

/// Callback for stderr output
///
/// This callback is invoked when the Claude CLI writes to stderr.
/// Useful for debugging and logging purposes.
pub type StderrCallback = Arc<dyn Fn(String) + Send + Sync>;

/// Callback for each raw stdout line, before it is parsed (without its line ending)
pub type StdoutCallback = Arc<dyn Fn(String) + Send + Sync>;

// ============================================================================
// Output Format
// ============================================================================

/// Output format configuration for structured outputs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFormat {
    /// Format type (always "`json_schema`")
    #[serde(rename = "type")]
    pub format_type: String,
    /// JSON schema for the output
    pub schema: serde_json::Value,
}

impl OutputFormat {
    /// Create a new JSON schema output format
    #[must_use]
    pub fn json_schema(schema: serde_json::Value) -> Self {
        Self {
            format_type: "json_schema".to_string(),
            schema,
        }
    }
}

// ============================================================================
// Sandbox Settings
// ============================================================================

/// Network-specific configuration for sandbox mode
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSandboxSettings {
    /// Allow processes to bind to local ports
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    /// Unix socket paths that processes can access
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Allow access to all Unix sockets
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    /// HTTP proxy port for network requests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<u16>,
    /// SOCKS proxy port for network requests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<u16>,
}

/// Configuration for ignoring specific sandbox violations
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxIgnoreViolations {
    /// File path patterns to ignore violations for
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<Vec<String>>,
    /// Network patterns to ignore violations for
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
}

/// Configuration for sandbox behavior
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// Enable sandbox mode for command execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Auto-approve bash commands when sandbox is enabled
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Commands that always bypass sandbox restrictions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_commands: Option<Vec<String>>,
    /// Allow the model to request running commands outside the sandbox
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    /// Network-specific sandbox configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkSandboxSettings>,
    /// Configure which sandbox violations to ignore
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<SandboxIgnoreViolations>,
    /// Enable a weaker nested sandbox for compatibility
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
}

// ============================================================================
// Plugin Configuration
// ============================================================================

/// Configuration for loading plugins
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SdkPluginConfig {
    /// Local plugin from filesystem path
    #[serde(rename = "local")]
    Local {
        /// Path to the plugin directory
        path: String,
    },
}

// ============================================================================
// Beta Features
// ============================================================================

/// Available beta features
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SdkBeta {
    /// 1 million token context window (compatible with Claude Sonnet 4, Claude Sonnet 4.5)
    #[serde(rename = "context-1m-2025-08-07")]
    Context1M,
}

impl SdkBeta {
    /// The value for `--betas`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Context1M => "context-1m-2025-08-07",
        }
    }
}

// ============================================================================
// Effort, thinking and permission prompts
// ============================================================================

/// Effort level for the session (`--effort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// `low`
    Low,
    /// `medium`
    Medium,
    /// `high`
    High,
    /// `xhigh`
    XHigh,
    /// `max`
    Max,
}

impl Effort {
    /// The value for `--effort`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = crate::ClaudeError;

    fn from_str(value: &str) -> crate::Result<Self> {
        Ok(match value {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => {
                return Err(crate::ClaudeError::invalid_config(format!(
                    "unknown effort '{value}': expected low, medium, high, xhigh or max"
                )));
            }
        })
    }
}

/// Extended thinking mode (`--thinking`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// `enabled`
    Enabled,
    /// `adaptive`: the model decides how much to think
    Adaptive,
    /// `disabled`
    Disabled,
}

impl ThinkingMode {
    /// The value for `--thinking`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Adaptive => "adaptive",
            Self::Disabled => "disabled",
        }
    }
}

/// Who answers permission prompts in print mode (`--permission-prompts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPrompts {
    /// The SDK host or `--permission-prompt-tool` answers (the CLI default)
    Host,
    /// Nobody: anything that would prompt is denied automatically
    None,
}

impl PermissionPrompts {
    /// The value for `--permission-prompts`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::None => "none",
        }
    }
}

// ============================================================================
// Authentication and environment
// ============================================================================

/// How the CLI process authenticates.
///
/// With [`ClaudeAuth::ApiKey`] or [`ClaudeAuth::OAuthToken`] the process receives exactly that
/// one credential variable; `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` and
/// `CLAUDE_CODE_OAUTH_TOKEN` inherited from the parent are dropped, since Claude Code ranks
/// them above a subscription token. The secret never appears in `Debug` output.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum ClaudeAuth {
    /// Use whatever the environment and the CLI's own login provide (the default)
    #[default]
    Inherit,
    /// An Anthropic API key, passed as `ANTHROPIC_API_KEY`
    ApiKey(String),
    /// A long-lived subscription token from `claude setup-token`, passed as
    /// `CLAUDE_CODE_OAUTH_TOKEN`
    OAuthToken(String),
}

impl std::fmt::Debug for ClaudeAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Inherit => "Inherit",
            Self::ApiKey(_) => "ApiKey(<redacted>)",
            Self::OAuthToken(_) => "OAuthToken(<redacted>)",
        })
    }
}

impl ClaudeAuth {
    /// Environment variables that carry a credential.
    pub const CREDENTIAL_VARS: [&'static str; 3] = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ];

    /// The variable and value this credential sets, if any.
    #[must_use]
    pub fn env_var(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Inherit => None,
            Self::ApiKey(key) => Some(("ANTHROPIC_API_KEY", key)),
            Self::OAuthToken(token) => Some(("CLAUDE_CODE_OAUTH_TOKEN", token)),
        }
    }
}

/// Which of the parent's environment variables the CLI process inherits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnvInheritance {
    /// Every variable (the default)
    #[default]
    All,
    /// Only the named variables. An entry ending in `*` is a prefix (`CLAUDE_*`). Names compare
    /// case-insensitively on Windows. `ClaudeAgentOptions::env`, the credential and the SDK's
    /// own variables are added on top.
    Only(Vec<String>),
}

impl EnvInheritance {
    /// Whether the parent variable `name` is inherited.
    #[must_use]
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(patterns) => {
                patterns
                    .iter()
                    .any(|pattern| match pattern.strip_suffix('*') {
                        Some(prefix) => name
                            .get(..prefix.len())
                            .is_some_and(|head| env_name_eq(head, prefix)),
                        None => env_name_eq(name, pattern),
                    })
            }
        }
    }
}

/// Whether two environment variable names are the same on this platform.
#[must_use]
pub fn env_name_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

// ============================================================================
// Agent Definition
// ============================================================================

/// Agent definition configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Agent description
    pub description: String,
    /// Agent system prompt
    pub prompt: String,
    /// Tools available to the agent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Model to use for the agent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ============================================================================
// Claude Agent Options
// ============================================================================

/// Main options for Claude Agent SDK
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Default, TypedBuilder)]
#[builder(
    builder_method(doc = "Create a new builder for ClaudeAgentOptions"),
    builder_type(doc = "Builder for ClaudeAgentOptions", vis = "pub"),
    build_method(doc = "Build the ClaudeAgentOptions")
)]
pub struct ClaudeAgentOptions {
    /// List of tools that Claude is allowed to use
    #[builder(default, setter(into))]
    pub allowed_tools: Vec<ToolName>,

    /// System prompt configuration
    #[builder(default, setter(strip_option, into))]
    pub system_prompt: Option<SystemPrompt>,

    /// Additional text to append to the system prompt (uses `--append-system-prompt` CLI flag)
    #[builder(default, setter(strip_option, into))]
    pub append_system_prompt: Option<String>,

    /// MCP server configurations
    #[builder(default)]
    pub mcp_servers: McpServers,

    /// Permission mode for tool execution
    #[builder(default, setter(strip_option))]
    pub permission_mode: Option<PermissionMode>,

    /// Whether to continue from the previous conversation
    #[builder(default)]
    pub continue_conversation: bool,

    /// Session ID to resume from
    #[builder(default, setter(strip_option, into))]
    pub resume: Option<super::identifiers::SessionId>,

    /// Maximum number of turns before stopping
    #[builder(default, setter(strip_option))]
    pub max_turns: Option<u32>,

    /// List of tools that Claude is not allowed to use
    #[builder(default, setter(into))]
    pub disallowed_tools: Vec<ToolName>,

    /// AI model to use
    #[builder(default, setter(strip_option, into))]
    pub model: Option<String>,

    /// Tool name to use for permission prompts
    #[builder(default, setter(strip_option, into))]
    pub permission_prompt_tool_name: Option<String>,

    /// Working directory for the CLI process
    #[builder(default, setter(strip_option, into))]
    pub cwd: Option<PathBuf>,

    /// Path to settings file
    #[builder(default, setter(strip_option, into))]
    pub settings: Option<PathBuf>,

    /// Additional directories to add to the context
    #[builder(default, setter(into))]
    pub add_dirs: Vec<PathBuf>,

    /// Environment variables for the CLI process
    #[builder(default)]
    pub env: HashMap<String, String>,

    /// Extra CLI arguments to pass
    #[builder(default)]
    pub extra_args: HashMap<String, Option<String>>,

    /// Maximum length of one output line, stdout or stderr (default: 1MB). A longer line is
    /// drained without being buffered and reported as a decode error.
    #[builder(default, setter(strip_option))]
    pub max_buffer_size: Option<usize>,

    /// Read timeout in seconds for waiting on CLI responses (default: 120s)
    /// Set higher for complex operations like subagent research.
    /// Set to 0 for no timeout.
    #[builder(default, setter(strip_option))]
    pub read_timeout_secs: Option<u64>,

    /// Callback for tool permission checks
    #[builder(default, setter(strip_option))]
    pub can_use_tool: Option<CanUseToolCallback>,

    /// Hook configurations
    #[builder(default, setter(strip_option))]
    pub hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,

    /// User identifier
    ///
    /// **Deprecated:** Claude Code has no `--user` flag (2.1.292 rejects it), so this value is
    /// no longer passed to the CLI. It is kept for source compatibility only.
    #[builder(default, setter(strip_option, into))]
    pub user: Option<String>,

    /// Whether to include partial messages in stream
    #[builder(default)]
    pub include_partial_messages: bool,

    /// Whether to fork the session when resuming
    #[builder(default)]
    pub fork_session: bool,

    /// Custom session ID (must be valid UUID)
    /// Used with --resume or --continue and --fork-session
    #[builder(default, setter(strip_option, into))]
    pub session_id: Option<String>,

    /// Custom agent definitions
    #[builder(default, setter(strip_option))]
    pub agents: Option<HashMap<String, AgentDefinition>>,

    /// Setting sources to load
    #[builder(default, setter(strip_option))]
    pub setting_sources: Option<Vec<SettingSource>>,

    // ========================================================================
    // New options for TypeScript SDK parity
    // ========================================================================
    /// Maximum budget in USD for the query
    #[builder(default, setter(strip_option))]
    pub max_budget_usd: Option<f64>,

    /// Maximum tokens for thinking process
    #[builder(default, setter(strip_option))]
    pub max_thinking_tokens: Option<u32>,

    /// Model to use if primary fails
    #[builder(default, setter(strip_option, into))]
    pub fallback_model: Option<String>,

    /// Output format for structured outputs (JSON schema)
    #[builder(default, setter(strip_option))]
    pub output_format: Option<OutputFormat>,

    /// Sandbox configuration for command execution
    #[builder(default, setter(strip_option))]
    pub sandbox: Option<SandboxSettings>,

    /// Plugins to load from local paths
    #[builder(default, setter(strip_option))]
    pub plugins: Option<Vec<SdkPluginConfig>>,

    /// Beta features to enable
    #[builder(default, setter(strip_option))]
    pub betas: Option<Vec<SdkBeta>>,

    /// Enforce strict MCP configuration validation
    #[builder(default)]
    pub strict_mcp_config: bool,

    /// Enable file checkpointing for rewind support
    ///
    /// When enabled, tracks file changes made through Write/Edit/NotebookEdit tools,
    /// allowing programmatic rewind via `rewind_files()`. Requires `--replay-user-messages`
    /// to capture checkpoint UUIDs from user messages.
    #[builder(default)]
    pub enable_file_checkpointing: bool,

    /// Resume session at a specific message UUID
    #[builder(default, setter(strip_option, into))]
    pub resume_session_at: Option<String>,

    // ========================================================================
    // Additional TypeScript SDK parity options
    // ========================================================================
    /// Enable bypassing permissions (requires permissionMode: `BypassPermissions`)
    ///
    /// **WARNING**: This is dangerous and should only be used in controlled
    /// environments. When true, allows using `PermissionMode::BypassPermissions`.
    #[builder(default)]
    pub allow_dangerously_skip_permissions: bool,

    /// Path to custom Claude Code executable
    ///
    /// If not specified, uses the bundled or globally installed executable.
    #[builder(default, setter(strip_option, into))]
    pub path_to_claude_code_executable: Option<PathBuf>,

    /// Callback for stderr output
    ///
    /// Invoked when the Claude CLI writes to stderr. Useful for debugging.
    #[builder(default, setter(strip_option))]
    pub stderr: Option<StderrCallback>,

    /// Callback for stdout output
    ///
    /// Invoked with every line the CLI writes to stdout, as written, before it is parsed (for
    /// a raw log). Messages are still parsed and delivered as usual.
    #[builder(default, setter(strip_option))]
    pub stdout: Option<StdoutCallback>,

    /// How the CLI process is started (default: [`crate::transport::DirectSpawner`])
    ///
    /// A spawner may contain the process (a Job Object, a process group) or start it through a
    /// wrapper; the SDK still builds the command and owns the process's pipes and exit.
    #[builder(default, setter(strip_option))]
    pub spawner: Option<crate::transport::SharedSpawner>,

    /// Tools configuration
    ///
    /// Either a list of tool names or a preset (e.g., `ToolsConfig::claude_code_preset()`).
    #[builder(default, setter(strip_option))]
    pub tools: Option<ToolsConfig>,

    // ========================================================================
    // Claude Code 2.1.x options
    // ========================================================================
    /// Effort level (`--effort low|medium|high|xhigh|max`)
    #[builder(default, setter(strip_option))]
    pub effort: Option<Effort>,

    /// Context window variant of `model`, e.g. `"1m"` for `opus[1m]`. Requires `model`.
    #[builder(default, setter(strip_option, into))]
    pub model_context: Option<String>,

    /// Extended thinking mode (`--thinking enabled|adaptive|disabled`)
    #[builder(default, setter(strip_option))]
    pub thinking: Option<ThinkingMode>,

    /// Who answers permission prompts (`--permission-prompts host|none`)
    #[builder(default, setter(strip_option))]
    pub permission_prompts: Option<PermissionPrompts>,

    /// Forward subagents' text and thinking as messages tagged with `parent_tool_use_id`
    /// (`--forward-subagent-text`)
    #[builder(default)]
    pub forward_subagent_text: bool,

    /// Emit every hook lifecycle event in the stream (`--include-hook-events`)
    #[builder(default)]
    pub include_hook_events: bool,

    /// Do not save the session to disk; it cannot be resumed (`--no-session-persistence`)
    #[builder(default)]
    pub no_session_persistence: bool,

    /// Display name for the session (`--name`)
    #[builder(default, setter(strip_option, into))]
    pub session_name: Option<String>,

    /// Agent for the session (`--agent`)
    #[builder(default, setter(strip_option, into))]
    pub agent: Option<String>,

    /// Minimal mode: skip hooks, plugins, auto-memory and CLAUDE.md discovery (`--bare`).
    /// Anthropic auth is then strictly `ANTHROPIC_API_KEY`.
    #[builder(default)]
    pub bare: bool,

    /// Restricted mode (`--restricted`): no command-running tools unless `tools` names them,
    /// no user/project/local settings, file tools confined to the working directories.
    #[builder(default)]
    pub restricted: bool,

    /// Move per-machine system prompt sections into the first user message for prompt-cache
    /// reuse (`--exclude-dynamic-system-prompt-sections`)
    #[builder(default)]
    pub exclude_dynamic_system_prompt_sections: bool,

    /// How the process authenticates (API key, `claude setup-token` token, or inherited)
    #[builder(default)]
    pub auth: ClaudeAuth,

    /// Claude Code's configuration folder for this process (`CLAUDE_CONFIG_DIR`): where it keeps
    /// settings, credentials and session transcripts (used by resume)
    #[builder(default, setter(strip_option, into))]
    pub config_dir: Option<PathBuf>,

    /// Which parent environment variables the process inherits (default: all)
    #[builder(default)]
    pub env_inheritance: EnvInheritance,
}

impl ClaudeAgentOptions {
    /// Maximum allowed turns
    pub const MAX_ALLOWED_TURNS: u32 = 1000;
}

#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for ClaudeAgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgentOptions")
            .field("allowed_tools", &self.allowed_tools)
            .field("system_prompt", &self.system_prompt)
            .field("mcp_servers", &self.mcp_servers)
            .field("permission_mode", &self.permission_mode)
            .field("continue_conversation", &self.continue_conversation)
            .field("resume", &self.resume)
            .field("max_turns", &self.max_turns)
            .field("disallowed_tools", &self.disallowed_tools)
            .field("model", &self.model)
            .field(
                "permission_prompt_tool_name",
                &self.permission_prompt_tool_name,
            )
            .field("cwd", &self.cwd)
            .field("settings", &self.settings)
            .field("add_dirs", &self.add_dirs)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("extra_args", &self.extra_args)
            .field("max_buffer_size", &self.max_buffer_size)
            .field(
                "can_use_tool",
                &self.can_use_tool.as_ref().map(|_| "<callback>"),
            )
            .field(
                "hooks",
                &self
                    .hooks
                    .as_ref()
                    .map(|h| format!("[{} hook types]", h.len())),
            )
            .field("user", &self.user)
            .field("include_partial_messages", &self.include_partial_messages)
            .field("fork_session", &self.fork_session)
            .field("session_id", &self.session_id)
            .field("agents", &self.agents)
            .field("setting_sources", &self.setting_sources)
            // New fields for TypeScript SDK parity
            .field("max_budget_usd", &self.max_budget_usd)
            .field("max_thinking_tokens", &self.max_thinking_tokens)
            .field("fallback_model", &self.fallback_model)
            .field("output_format", &self.output_format)
            .field("sandbox", &self.sandbox)
            .field(
                "plugins",
                &self
                    .plugins
                    .as_ref()
                    .map(|p| format!("[{} plugins]", p.len())),
            )
            .field("betas", &self.betas)
            .field("strict_mcp_config", &self.strict_mcp_config)
            .field("resume_session_at", &self.resume_session_at)
            // Additional TypeScript SDK parity options
            .field(
                "allow_dangerously_skip_permissions",
                &self.allow_dangerously_skip_permissions,
            )
            .field(
                "path_to_claude_code_executable",
                &self.path_to_claude_code_executable,
            )
            .field("stderr", &self.stderr.as_ref().map(|_| "<callback>"))
            .field("stdout", &self.stdout.as_ref().map(|_| "<callback>"))
            .field("spawner", &self.spawner.as_ref().map(|_| "<spawner>"))
            .field(
                "tools",
                &self.tools.as_ref().map(|t| match t {
                    ToolsConfig::List(l) => format!("[{} tools]", l.len()),
                    ToolsConfig::Preset(p) => format!("preset:{}", p.preset),
                }),
            )
            .field("effort", &self.effort)
            .field("model_context", &self.model_context)
            .field("thinking", &self.thinking)
            .field("permission_prompts", &self.permission_prompts)
            .field("forward_subagent_text", &self.forward_subagent_text)
            .field("include_hook_events", &self.include_hook_events)
            .field("no_session_persistence", &self.no_session_persistence)
            .field("session_name", &self.session_name)
            .field("agent", &self.agent)
            .field("bare", &self.bare)
            .field("restricted", &self.restricted)
            .field(
                "exclude_dynamic_system_prompt_sections",
                &self.exclude_dynamic_system_prompt_sections,
            )
            .field("auth", &self.auth)
            .field("config_dir", &self.config_dir)
            .field("env_inheritance", &self.env_inheritance)
            .finish()
    }
}
