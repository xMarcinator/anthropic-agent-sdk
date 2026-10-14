# Using anthropic-agent-sdk 0.3.0 in flow-next-tracker

This note compares the SDK (branch `update-claude-2.1.292`, version 0.3.0) with the tracker's
Claude provider in `flow-next-tracker-fn2/src/runner/providers/claude.rs` and the stream-JSON
handling around it (`provider.rs`, `launcher.rs`, `process.rs`). Everything below was checked
against Claude Code 2.1.292. The real CLI was driven through the SDK with an empty
`CLAUDE_CONFIG_DIR` and no credential, so it answered "Not logged in" and made no model call.
That run confirmed the init event, the replay echo, `command_lifecycle`, the assistant
`authentication_failed` error, the result fields, the interrupt `control_response`,
`initialize`, `set_model` and `set_permission_mode`.

## What the SDK can replace

| Tracker today (`providers/claude.rs`) | SDK 0.3.0 replacement |
| --- | --- |
| `agent_args`: the fixed flag list (`-p`, stream-JSON in and out, `--verbose`, `--replay-user-messages`, `--forward-subagent-text`, `--permission-mode`, `--permission-prompts host`, `--permission-prompt-tool mcp__…__permission_prompt`, `--disallowedTools AskUserQuestion`, `--strict-mcp-config`, `--mcp-config <file>`, `--session-id`/`--resume`, `--add-dir`) | `ClaudeAgentOptions` plus `SubprocessTransport::command_args()`, which returns the argv without starting anything. Every flag has a typed option: `permission_mode`, `permission_prompts`, `permission_prompt_tool_name`, `forward_subagent_text`, `disallowed_tools`, `strict_mcp_config`, `mcp_servers: McpServers::Path`, `session_id`/`resume`, `add_dirs`. |
| `agent_choice_args`: `--model {model}[{context}]`, `--effort` | `model` + `model_context` (same "context needs a model" error), `effort: Effort` (`low`…`max`). |
| `child_env`: the allow-list plus `ANTHROPIC_*`/`CLAUDE_*` prefixes, plus `MCP_TOOL_TIMEOUT` | `env_inheritance: EnvInheritance::Only([... , "ANTHROPIC_*", "CLAUDE_*"])` (case-insensitive on Windows, like the tracker), `env` for `MCP_TOOL_TIMEOUT`, all through `command_env(host_env)`. |
| fn-2.26 credential handling: exactly one of `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN`, inherited ones (and `ANTHROPIC_AUTH_TOKEN`) dropped | `auth: ClaudeAuth::{ApiKey, OAuthToken, Inherit}` does exactly that and redacts the secret from `Debug`. |
| fn-2 R59 per-session Claude folder | `config_dir` (`CLAUDE_CONFIG_DIR`). |
| `user_line` (user message with `uuid`) | `ClaudeSDKClient::send_message_with_uuid`, or the same JSON shape. |
| `interrupt_line` | `ControlRequestEnvelope::new(id, ControlRequestBody::interrupt()).to_line()`; the answer parses as `ControlResponseBody` with `still_queued()`. |
| `StreamParser::line` JSON matching (`system`/`init`, `assistant`, `user` with `isReplay`/`isSynthetic`/`uuid`, `control_response`, `result`) | `parse_message` → `Message`. Unknown types and blocks come back as `Message::Other` / `ContentBlock::Other` instead of being counted as skipped. `Message::as_system_init()` gives `model`, `apiKeySource`, `session_id`, `claude_code_version` and `capabilities`. |
| `result_usage` (per-turn `usage`, `modelUsage`, `total_cost_usd`) | `Message::Result { model_usage: HashMap<String, ModelUsage>, total_cost_usd, .. }` and `Message::result_usage() -> Usage`. The summing policy (per-model totals, else the sum of turns, and the 2.1.277 resume rule) stays in the tracker. |
| Not parsed today: `rate_limit_event` (needed by fn-2.60), `command_lifecycle`, the assistant `error` field (needed by fn-2.26 to spot a rejected credential) | `Message::RateLimitEvent` with `RateLimitInfo::window()` for `five_hour`, `seven_day` and model-scoped windows (unknown names are kept), `Message::CommandLifecycle` (`queued`/`started`/`cancelled` per user `uuid`), `Message::Assistant { error: Some("authentication_failed"), .. }`. |

The tracker's own logic stays where it is: chat entries, tool summaries, subagent grouping
(`parent_tool_use_id`), `ended_with_text`, the version gates and the usage policy. Only the
wire format moves into the SDK.

## What the tracker needs that the SDK still lacks

1. **A client that does not own the process.** `ClaudeSDKClient` is hard-wired to
   `SubprocessTransport`, which spawns with `tokio::process::Command`: no Job Object or process
   group, only the direct child is killed on drop, and the stdout lines are not capped. The
   runner's `Launcher`/`Tree` must keep owning the process, so the tracker can use the SDK's
   pure parts (`command_args`, `command_env`, `parse_message`, `control::wire`) but not the
   client. The missing piece is a client over caller-provided pipes, something like
   `ClaudeSDKClient::from_io(stdin, stdout)` or a sans-IO session state machine.
2. **Omitting `--setting-sources`.** The SDK always passes `--setting-sources ""` when
   `setting_sources` is `None`, so no user, project or local settings load. The tracker's local
   runs pass no flag and load the user's own settings (fn-2 R17). An explicit
   `Some(vec![User, Project, Local])` comes close but is not the same as leaving the flag out.
   The SDK needs a way to omit it.
3. **The SDK's own environment variables.** `command_env` always sets
   `CLAUDE_CODE_ENTRYPOINT=sdk-rust`, `CLAUDE_AGENT_SDK_VERSION` and `PWD`. The tracker sets
   none of them today, and the entrypoint can change how Claude Code labels and handles the
   session. The SDK needs an opt-out.
4. **Line-limit semantics.** The tracker skips lines over its limit (`cut`) and keeps going.
   The SDK accumulates partial JSON up to `max_buffer_size` and then reports an error. Behind
   the runner's reader only `parse_message` is used, so this matters only if the SDK reader is
   ever adopted.
5. **Version helpers.** The SDK has `MIN_CLI_VERSION`/`TESTED_CLI_VERSION` and
   `check_claude_version`, but no `at_least(version, (2, 1, 277))` comparison. The tracker keeps
   its own.
6. **Redaction.** The SDK never logs argv or env, but it does not redact secrets from output
   lines. The tracker's planned `Redactor` (fn-2.26) still runs before parsing.
7. **Not implemented.** Hook callbacks registered via `initialize` (`hook_callback`) and SDK
   MCP servers over `mcp_message` are both answered with an error. The tracker uses neither.

## Integration risks

- **Process ownership by the runner.** Adopting the SDK client as it is would move spawning out
  of the launcher, the only code allowed to start processes. It would also lose the Job Object /
  process group (grandchildren such as MCP servers and Bash tools would survive a stop), the
  suspended start and the spool. Use the SDK only to build the argv and env and to parse lines
  until gap 1 is closed.
- **Env allow-listing.** `EnvInheritance::Only` matches the tracker's list and prefixes, but the
  SDK adds its own variables (gap 3). It also rejects `PATH`, `NODE_OPTIONS`, `LD_PRELOAD` and
  similar names in `options.env`, so a remote runner that needs to set `PATH` explicitly must
  inherit it instead. With `ClaudeAuth::Inherit` and the `ANTHROPIC_*` prefix allowed, inherited
  credentials still pass, as they do today. Golden env tests should compare
  `command_env(host_env)` with today's `child_env` output.
- **Per-session MCP endpoint.** The tracker writes its per-session HTTP MCP config to a file
  (`--mcp-config <path>`). `McpServers::Dict` would instead serialize the config, including the
  session URL with its token, into argv, where the process list shows it. Keep
  `McpServers::Path` and the runner-written file, together with `strict_mcp_config`.
- **The `permission_prompt` tool.** Today a permission request goes from Claude through MCP to
  the tracker's `permission_prompt` tool, and `MCP_TOOL_TIMEOUT=2400000` lets it outlast the
  30-minute wait. The SDK also supports `--permission-prompt-tool stdio` (a `can_use_tool`
  callback answered over stdin), which would skip the MCP round-trip, but that is a behaviour
  change. The MCP tool timeout would no longer apply, the decision would move into the process
  that owns stdin, and nobody has checked how long the CLI waits on a stdio prompt. Keep the MCP
  tool for now. Pass `permission_prompt_tool_name` and `PermissionPrompts::Host`, and do not set
  `can_use_tool`: the SDK refuses to have both.
- **Forward compatibility of the board's stats.** Lines the SDK now parses (`command_lifecycle`,
  `rate_limit_event`, `system`/`status`) are counted as `skipped` by the tracker today. Switching
  will lower the skipped counts, so tests that assert them need updating.
- **Permission mode names.** 2.1.292 lists `manual` instead of `default` on `--permission-mode`
  but still accepts `default` and reports `default` in the init event. On the control channel
  `set_permission_mode` refuses `manual`. `PermissionMode::{Default, Manual}` handle both;
  fn-2 R36's "Ask" mode should map to `Default`. `auto` can be refused per model ("auto mode
  unavailable for this model" for Haiku), so R36's version/mode refusal also needs a
  model check.

## Suggested integration task

**Use anthropic-agent-sdk for Claude Code's wire format in the runner's Claude provider**
(the runner keeps process ownership)

- `providers/claude.rs` builds its argv and environment through `ClaudeAgentOptions` and
  `SubprocessTransport::command_args`/`command_env`. The golden tests for args and env still
  pass unchanged, which needs the SDK options to omit `--setting-sources` and the SDK's own
  env variables (gaps 2 and 3, fixed in the SDK first).
- `StreamParser` parses each line with `parse_message`, and every existing parser test passes.
  The skipped count no longer includes `command_lifecycle`, `rate_limit_event` or `system`
  status lines.
- `interrupt_line` and `message_line` produce their lines with `control::wire` and the SDK's
  user-message shape, and a fake-CLI test covers the interrupt and replay round-trips, including
  the `still_queued` answer.
- `rate_limit_event` windows (`five_hour`, `seven_day`, model-scoped) and the assistant
  `authentication_failed` error are surfaced to the runner for fn-2.60 and fn-2.26, with tests
  built from 2.1.292 fixtures.
- No process is started outside `Launcher`, the MCP config stays a runner-written file, and the
  `permission_prompt` MCP tool and `MCP_TOOL_TIMEOUT` stay as they are.

## Update: the tracker runs on `ClaudeSDKClient`

The tracker chose the client after all (fn-2.102): the SDK owns the process and the protocol.
The gaps above that blocked it are closed on this branch: `spawner` lets the runner start the
process inside its Job Object or process group, `process()` gives it the exit code and a kill
independent of the message stream, `end_input()` closes stdin, `stdout` feeds its raw log, a
line that is not JSON is skipped, and `can_use_tool` callbacks run concurrently and are dropped
when the CLI withdraws a request. Permission prompts now come through `can_use_tool`
(`--permission-prompt-tool stdio`); the tracker's `permission_prompt` MCP tool is gone. The
tracker passes `setting_sources: [user, project, local]`, so its sessions still load the user's
settings, and accepts the SDK's own environment variables.
