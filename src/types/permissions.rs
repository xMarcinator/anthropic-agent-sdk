//! Permission types for tool execution control

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::identifiers::ToolName;

// ============================================================================
// Permission Types
// ============================================================================

/// Permission modes for tool execution
///
/// Claude Code 2.1.292 accepts `acceptEdits`, `auto`, `bypassPermissions`, `manual`, `dontAsk`
/// and `plan` on `--permission-mode`, plus the older `default` (an alias of `manual` that the
/// CLI still reports in its init message).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionMode {
    /// Default mode - CLI prompts for dangerous tools (`default`, same as [`Self::Manual`])
    Default,
    /// Ask for every action no rule decides (`manual`; reported back as `default`)
    Manual,
    /// Auto-accept file edits
    AcceptEdits,
    /// Plan mode
    Plan,
    /// Claude Code's auto mode: a classifier approves routine actions and asks for the rest
    Auto,
    /// Deny anything that would prompt, without asking
    DontAsk,
    /// Allow all tools (use with caution)
    BypassPermissions,
}

impl PermissionMode {
    /// The value for `--permission-mode`.
    #[must_use]
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Manual => "manual",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// The value for the `set_permission_mode` control request, which rejects `manual` and
    /// takes `default` for it.
    #[must_use]
    pub fn control_value(self) -> &'static str {
        match self {
            Self::Manual => "default",
            other => other.cli_value(),
        }
    }

    /// Parses a mode as the CLI reports it (`permissionMode` in the init message).
    #[must_use]
    pub fn from_cli_value(value: &str) -> Option<Self> {
        Some(match value {
            "default" => Self::Default,
            "manual" => Self::Manual,
            "acceptEdits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "auto" => Self::Auto,
            "dontAsk" => Self::DontAsk,
            "bypassPermissions" => Self::BypassPermissions,
            _ => return None,
        })
    }
}

/// Setting source types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// User-level settings
    User,
    /// Project-level settings
    Project,
    /// Local settings
    Local,
}

/// Permission update destination
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// Save to user settings
    UserSettings,
    /// Save to project settings
    ProjectSettings,
    /// Save to local settings
    LocalSettings,
    /// Save to session only (temporary)
    Session,
}

/// Permission behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Allow the action
    Allow,
    /// Deny the action
    Deny,
    /// Ask the user
    Ask,
}

/// Permission rule value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Name of the tool
    pub tool_name: String,
    /// Optional rule content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// Permission update configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PermissionUpdate {
    /// Add permission rules
    AddRules {
        /// Rules to add
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Where to save the rules
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Replace existing permission rules
    ReplaceRules {
        /// New rules
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Where to save the rules
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Remove permission rules
    RemoveRules {
        /// Rules to remove
        #[serde(skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<PermissionRuleValue>>,
        /// Where to remove from
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Set permission mode
    SetMode {
        /// New permission mode
        mode: PermissionMode,
        /// Where to save the mode
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Add directories to allowed list
    AddDirectories {
        /// Directories to add
        #[serde(skip_serializing_if = "Option::is_none")]
        directories: Option<Vec<String>>,
        /// Where to save
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Remove directories from allowed list
    RemoveDirectories {
        /// Directories to remove
        #[serde(skip_serializing_if = "Option::is_none")]
        directories: Option<Vec<String>>,
        /// Where to remove from
        #[serde(skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
}

/// Context for tool permission callbacks
///
/// Provides permission suggestions and cancellation support to permission callbacks.
/// Equivalent to TypeScript SDK's `{ signal: AbortSignal, suggestions?: PermissionUpdate[] }`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ToolPermissionContext {
    /// Permission suggestions from CLI
    pub suggestions: Vec<PermissionUpdate>,
    /// Cancellation token for aborting operations (like `AbortSignal` in JS)
    #[serde(skip)]
    pub cancellation_token: Option<CancellationToken>,
}

impl std::fmt::Debug for ToolPermissionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolPermissionContext")
            .field("suggestions", &self.suggestions)
            .field(
                "cancellation_token",
                &self.cancellation_token.as_ref().map(|_| "<token>"),
            )
            .finish()
    }
}

impl ToolPermissionContext {
    /// Create a new `ToolPermissionContext` with suggestions
    #[must_use]
    pub fn new(suggestions: Vec<PermissionUpdate>) -> Self {
        Self {
            suggestions,
            cancellation_token: None,
        }
    }

    /// Create a new `ToolPermissionContext` with suggestions and cancellation token
    #[must_use]
    pub fn with_cancellation(
        suggestions: Vec<PermissionUpdate>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            suggestions,
            cancellation_token: Some(cancellation_token),
        }
    }

    /// Check if cancellation has been requested
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    }
}

/// Permission request from CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Tool name being requested
    pub tool_name: ToolName,
    /// Tool input parameters
    pub tool_input: serde_json::Value,
    /// Permission context
    pub context: ToolPermissionContext,
}

/// Permission result for allowing tool use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResultAllow {
    /// Modified input for the tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    /// Permission updates to apply
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_permissions: Option<Vec<PermissionUpdate>>,
}

/// Permission result for denying tool use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResultDeny {
    /// Reason for denying
    pub message: String,
    /// Whether to interrupt the conversation
    #[serde(default)]
    pub interrupt: bool,
}

/// Permission result enum
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PermissionResult {
    /// Allow the tool use
    Allow(PermissionResultAllow),
    /// Deny the tool use
    Deny(PermissionResultDeny),
}

/// Type alias for a shared permission callback using the trait-based approach.
pub type CanUseToolCallback = Arc<dyn crate::callbacks::PermissionCallback>;
