# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

What a host that runs Claude Code sessions for others needs from `ClaudeSDKClient`.

### Added
- `ClaudeAgentOptions::spawner` (`ProcessSpawner`, `SpawnRequest`): the SDK still builds the
  command and owns the process, but the caller decides how it starts, for example inside a
  Windows Job Object or a Unix process group, or through a wrapper executable.
- `ClaudeSDKClient::process()` returns a `ProcessHandle` to wait for the process's exit code
  or kill it from any task, independent of the message stream.
- `ClaudeSDKClient::end_input()` closes stdin, so the CLI finishes and exits.
- `ClaudeAgentOptions::stdout`: a callback with every raw stdout line, for a log.

### Fixed
- A stdout line that is not JSON is reported as one `JsonDecode` error and skipped; it no
  longer swallows the lines after it or ends the client's message stream. Each line is parsed
  on its own: a truncated message (`{`) no longer absorbs the next line.
- `max_buffer_size` bounds every stdout and stderr line as it is read: a longer line is drained
  without being held in memory, its head goes to the raw callback, and it is reported as one
  `JsonDecode` error.
- The message stream ends only after the process's stderr has reached the `stderr` callback
  (at most 2 seconds after the process exits, when a grandchild still holds the pipe).
- `can_use_tool` callbacks run concurrently: the permission manager is no longer locked while
  a callback waits for its answer.
- A `control_cancel_request` from the CLI drops the pending `can_use_tool` callback's future
  instead of answering it later.
- Dropping the client no longer cuts off stderr: the last lines a process writes before it
  exits still reach the `stderr` callback.

## [0.3.0] - 2026-10-14

Brings the SDK up to Claude Code 2.1.292. Flags were checked against `claude --help` and the
CLI's argument parser; message and control shapes against its stream-JSON output.

### Fixed
- Flags the CLI rejects are no longer built: `--beta` is now `--betas`, `--resume-at` is now
  `--resume-session-at` (and requires `resume`), `--user` is dropped, and the invented
  `--sandbox*` flags are replaced by `--settings '{"sandbox": {...}}'` (camelCase keys).
- `allow_dangerously_skip_permissions` now passes `--allow-dangerously-skip-permissions`.
- `path_to_claude_code_executable` is now used when no explicit CLI path is given.
- The `stderr` callback is now called with each stderr line (it was never invoked).
- `can_use_tool` callbacks now run: the CLI is started with `--permission-prompt-tool stdio`
  and its `can_use_tool` control requests are answered with a `control_response`. Before, the
  CLI never asked and its control requests failed to parse.
- Control responses are correlated by `request_id` instead of being dropped; control requests
  from the CLI that the SDK cannot serve (`hook_callback`, `mcp_message`, unknown subtypes) are
  answered with an error so the CLI never waits.
- Message parsing is forward-compatible: unknown message types, unknown content blocks and
  known types whose payload changed no longer fail the stream.
- The CLI command is no longer logged with its arguments and environment (secrets).
- Unit and integration tests no longer start the real CLI or call a model; the real-CLI tests
  are `#[ignore]`d and the client tests run against a fake CLI.

### Added
- Options: `effort` (`Effort::{Low, Medium, High, XHigh, Max}`), `model_context` (`[1m]`
  suffix), `thinking` (`ThinkingMode`), `permission_prompts` (`PermissionPrompts`),
  `forward_subagent_text`, `include_hook_events`, `no_session_persistence`, `session_name`,
  `agent`, `bare`, `restricted`, `exclude_dynamic_system_prompt_sections`.
- Environment: `auth` (`ClaudeAuth::{Inherit, ApiKey, OAuthToken}`; a set credential replaces
  inherited `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` and `CLAUDE_CODE_OAUTH_TOKEN`),
  `config_dir` (`CLAUDE_CONFIG_DIR`), `env_inheritance` (`EnvInheritance::Only` allow-list with
  `PREFIX_*` patterns). `ClaudeAuth` and `env` values are redacted from `Debug`.
- `PermissionMode::{Manual, Auto, DontAsk}` plus `cli_value`, `control_value` and
  `from_cli_value`.
- `SubprocessTransport::command_args` and `command_env` expose the exact argv and environment.
- Messages: `Message::RateLimitEvent` (`RateLimitInfo`, `RateLimitType`, `RateLimitWindow` with
  `five_hour`, `seven_day`, model-scoped and future windows), `Message::CommandLifecycle`,
  `Message::ControlRequest`, `Message::ControlResponse`, `Message::ControlCancelRequest`,
  `Message::Other`; `ContentBlock::Other`; `SystemInit` (`Message::as_system_init`), typed
  `Usage` (`Message::result_usage`), `Message::message_type`, `Message::is_replay`.
- Fields: user `is_replay`, `is_synthetic`, `timestamp`; assistant `uuid`, `error`; assistant
  content `id`, `stop_reason`, `usage`; result `uuid`, `stop_reason`, `terminal_reason`,
  `user_message_uuids`, `extra` (every other field, e.g. `fast_mode_state`).
- `control::wire`: the real stream-JSON control protocol (`ControlRequestBody`,
  `ControlResponseBody`, envelopes, `CanUseToolRequest`, `InboundControlRequest`).
- Client: `send_message_with_uuid`, `send_control_request`, `interrupt_and_wait` (returns
  `still_queued`), `initialize`, `change_model`, `change_permission_mode`,
  `change_max_thinking_tokens`, `mcp_status`; `respond_to_permission` and `respond_to_hook`
  now write real `control_response` lines. `DEFAULT_CONTROL_TIMEOUT`, `TESTED_CLI_VERSION`.
- `tests/fake_cli.rs`: a fake CLI covering replay, interrupt, control and permission
  round-trips and `query` string mode.

### Changed (breaking)
- `Message`, `ContentBlock` and `PermissionMode` are `#[non_exhaustive]` and have new variants;
  exhaustive matches need a wildcard arm.
- New fields on `Message::User` (`is_replay`, `is_synthetic`, `timestamp`),
  `Message::Assistant` (`uuid`, `error`) and `Message::Result` (`uuid`, `stop_reason`,
  `terminal_reason`, `user_message_uuids`, `extra`) and on `AssistantMessageContent` (`id`,
  `stop_reason`, `usage`): struct literals must set them (`AssistantMessageContent::new` and
  `Default` help) and patterns without `..` must name them.
- `ClaudeAgentOptions` has new public fields; struct literals need `..Default::default()`.
- `SandboxSettings`, `NetworkSandboxSettings` and `SandboxIgnoreViolations` serialize with
  camelCase keys (the settings-file format).
- `parse_message` returns `Message::Other` instead of an error for unknown types.
- `ClaudeAgentOptions::user` is no longer passed to the CLI (it has no such flag).
- Setting both `sandbox` and `settings`, `model_context` without `model`,
  `resume_session_at` without `resume`, or `can_use_tool` together with
  `permission_prompt_tool_name` is now an `InvalidConfig` error.
- With a `can_use_tool` callback the CLI asks the callback for permission (previously the
  callback was silently unused).
- The CLI process gets an explicit environment (`env_clear` + the computed variables); with
  the default `EnvInheritance::All` the result is the same as before.

### Not yet supported
- Hook callbacks registered with the CLI through `initialize` (`hook_callback`); hooks are
  still simulated from the message stream.
- SDK MCP servers over `mcp_message`; such requests are answered with an error.

## [0.2.75] - 2025-12-22

### Added
- OAuth 2.0 authentication module with PKCE support for Claude Max/Pro subscribers
- `OAuthClient` for browser-based authentication without API keys
- `TokenStorage` for persistent token caching (platform-specific config directory)
- `TokenInfo` with expiration tracking and auto-refresh support
- `oauth_demo` example with status, logout, and help commands
- File checkpointing support via `enable_file_checkpointing` option (CLI 2.0.75+) [experimental]
- `uuid` field on `Message::User` for message tracking
- `RewindFiles` control request for checkpoint restoration [experimental]
- `rewind_files()` method on `ClaudeSDKClient` [experimental]
- `checkpointing_demo` and `checkpointing_rewind_test` examples [experimental]
- Hook input types: `PreCompactHookInput`, `CompactTrigger`, `PermissionRequestHookInput`
- `resume_session_at` option for resuming at specific message UUID

### Changed
- Token exchange uses JSON body format (not form-urlencoded) per Anthropic API
- Hook input types now use `Option<String>` for `tool_use_id` field (CLI 2.0.75+)

### Dependencies
- Added `sha2` 0.10.9 for PKCE challenge generation
- Added `base64` 0.22.1 for base64url encoding
- Added `reqwest` 0.12.28 for HTTP requests
- Added `dirs` 6.0.0 for config directory resolution

## [0.2.67] - 2025-12-12

### Added
- Interactive TUI demo crate at `demos/tui-demo` with Goose-style UI
- Spinner with cycling random thinking messages (updates every 2 seconds)
- Styled prompts: green `>>` for user input, cyan `claude>` for responses
- Tool use headers showing operation and file path
- Markdown rendering with syntax highlighting via bat
- REPL input with rustyline (history, multi-line, Ctrl+C/D handling)
- Slash commands: `/help`, `/quit`, `/clear`, `/test-spinner`
- Context usage visualization with colored dot progress indicators
- Goose TUI analysis documentation at `docs/goose-tui-analysis.md`

### Changed
- Converted project to workspace with SDK at root and demos as members
- Workspace resolver set to "2" for consistent dependency resolution

### Fixed
- Clippy error in `src/transport/mod.rs` (items after test module)
- Clippy errors in `examples/mcp_integration.rs` (collapsible match, single match)

## [0.2.60] - 2025-12-07

### Added
- Rust SDK for Claude Code CLI with full TypeScript SDK parity
- Async streaming queries via `query()` function
- Bidirectional client with `ClaudeSDKClient` for multi-turn conversations
- Message buffering via `queue_message()` and `next_buffered()` for CLI timing
- Session binding with auto-bind on first Result message
- Session resume support for model switching mid-conversation
- Interrupt support via `interrupt()` for stopping streaming responses
- Hook system with 11 event types (PreToolUse, PostToolUse, SessionStart, etc.)
- Permission system with `PermissionCallback` trait and `PermissionManager`
- MCP server integration (stdio, SSE, HTTP, SDK in-process via rmcp feature)
- Introspection methods: `session_info()`, `supported_models()`, `supported_commands()`, `mcp_server_status()`
- Result message fields: `model_usage`, `permission_denials`, `structured_output`, `errors`
- Runtime setters for model, max_thinking_tokens, permission_mode
- Security hardening: env var blocking, CLI arg allowlist, session validation
- UTF-8 safe string utilities: `safe_truncate()`, `truncate_for_display()`, `safe_window()`
- 26 working examples covering all SDK features
- 3 integration test suites (client, control protocol, security)
- Comprehensive API documentation (README_API.md)
- Security documentation with threat model (SECURITY.md)

### Features
- Full TypeScript SDK parity across all options, methods, and hook events
- Trait-based callbacks for hooks and permissions
- Type-safe identifiers (SessionId, ToolName, RequestId)
- Builder pattern via typed-builder for ergonomic configuration
- 100% safe Rust with zero unsafe blocks
- Async-first design with tokio runtime
- Streaming message parsing with futures::Stream
- Configurable timeouts and buffer limits
- CLI version validation (requires Claude Code 2.0.60+)
- Optional tracing support for structured logging
- Optional rmcp feature for in-process MCP SDK servers

### Security
- Environment variable filtering blocks LD_PRELOAD, PATH, NODE_OPTIONS, etc.
- CLI argument allowlist prevents injection attacks
- Session binding validates session IDs on send
- 30-second default I/O timeouts
- 1MB default buffer limits
- Strict error handling on security violations
