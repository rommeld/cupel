# cupel features

Inventory of the features implemented in the current source checkout, including
local working-tree changes, as of October 9, 2026. This describes implementation
scope, not a roadmap or a guarantee that every feature is in a published release.

## Contents

- [CLI and operating modes](#cli-and-operating-modes)
- [Providers, models, and authentication](#providers-models-and-authentication)
- [Coding tools and retrieval](#coding-tools-and-retrieval)
- [Agent execution and context management](#agent-execution-and-context-management)
- [Project customization](#project-customization)
- [Sessions and persistence](#sessions-and-persistence)
- [Parallel worktree sessions](#parallel-worktree-sessions)
- [Terminal UI](#terminal-ui)
- [Safety, hooks, and extensibility](#safety-hooks-and-extensibility)
- [Slash commands](#slash-commands)
- [Distribution and maintenance](#distribution-and-maintenance)
- [Current boundaries](#current-boundaries)

## CLI and operating modes

- Interactive terminal UI built with `ratatui`.
- Plain-text REPL through `--plain`.
- Automatic plain mode when stdin or stdout is not a terminal.
- Piped stdin submitted as one complete prompt, preserving multiline input.
- Startup model selection with `--model` or `-m`.
- Startup thinking selection with `--thinking` or `-t`.
- Resume the latest or a named session with `--resume` or `-r`.
- Offline `--help` listing built-in and file-configured models without a network
  probe.
- Graceful broken-pipe handling and signal-driven command cleanup in plain mode.
- Failure exit status for terminal model errors in plain mode.
- Interactive startup without credentials, allowing authentication or
  configuration afterward.

## Providers, models, and authentication

### Built-in providers

| Provider | Protocol or authentication |
| --- | --- |
| Anthropic | Messages API |
| OpenAI | Responses API |
| OpenAI Codex | ChatGPT OAuth and the Codex Responses backend |
| AWS Bedrock | ConverseStream and the AWS credential chain |
| Fireworks | OpenAI-compatible Chat Completions |
| OpenRouter | OpenAI-compatible Chat Completions gateway |

### Model configuration

- OpenAI-compatible custom and local endpoints, including Ollama, llama-server,
  LM Studio, and proxies.
- Automatic Ollama discovery with a bounded, fail-soft probe using `OLLAMA_HOST`
  or the default localhost endpoint.
- Discovered models appear in the runtime model and provider lists.
- Embedded model catalog with pricing, input modalities, reasoning support,
  context windows, optional context ceilings, and output-token limits.
- Global and project `models.json` additions and overrides without recompiling.
- Explicit model entries take precedence over automatically discovered models.
- Runtime model and provider switching.
- Named presets combining provider, model, thinking level, and an optional
  additional system prompt.
- A `default` preset for startup, with individual CLI overrides.
- Automatic startup selection using presets, stored Codex login, available
  credentials, or keyless local models.
- Thinking levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.
- Provider- and model-specific thinking mappings, adaptive/budget-based thinking,
  and clamping to supported levels.
- Compatibility settings for endpoint-specific request fields and capabilities.
- Custom model-level and request-level HTTP headers.

### Credentials

- API-key resolution: session-entered key, then environment variable, then home
  settings.
- Persistent provider keys in the home `settings.json`; project settings cannot
  supply provider keys.
- Codex browser OAuth, pasted-redirect fallback, and device-code login.
- Stored OAuth credentials in `auth.json` with automatic token refresh before
  requests.
- Login cancellation and logout.
- AWS credential-chain integration, including profile and region configuration.
- Keyless local endpoints.
- Atomic credential/settings replacement and owner-only Unix file permissions.

## Coding tools and retrieval

| Tool | Capabilities |
| --- | --- |
| `read` | Read text with offset/limit paging; attach JPG, JPEG, PNG, GIF, and WebP images; bound output and provide continuation notices. |
| `grep` | Regex or literal search; case-insensitive matching; path/glob filters; context lines; file paths and line numbers; `.gitignore` awareness. |
| `apply_patch` | Create, delete, update, and rename multiple files in one patch; validate all hunks before writing; preserve BOM/line endings; return change diffs. |
| `bash` | Execute shell commands; stream stdout/stderr; optional timeout; report nonzero exits as errors; preserve oversized output in temporary files; cancel process groups. |

Additional tooling features:

- Ranked `grep` files mode, prioritizing definitions and demoting test code.
- Best-match previews and bounded match counts per file.
- Deterministic search ordering, hidden-file inclusion, `.git` exclusion, and
  binary-file skipping.
- Bounded tool output to control memory use and context consumption.
- File-mutation locking for overlapping patches.
- Background shell processes can outlive the initial shell call without keeping
  the tool waiting indefinitely on inherited output pipes.
- `/review` bundles the project, selected files/directories, or `--diff` changes
  into a bounded code-review prompt requesting findings rather than edits.

## Agent execution and context management

- Repeated inference, tool execution, and result-feedback loop.
- Streaming text, reasoning, tool-call arguments, and tool-execution events.
- Parallel tool execution, with sequential execution supported by the library.
- Follow-up prompts queued while the agent works and processed one at a time
  after it would otherwise stop.
- Active-run steering: interrupt a streaming reply and deliver steering messages
  before the next request; tools already running finish first.
- Cancellation of active inference and tools.
- Automatic transient-error retries with exponential backoff.
- Non-retryable handling for invalid requests, billing/quota failures, and
  content-filter stops.
- Content-filter explanations while preserving partial output and usage.
- Automatic context compaction in two tiers:
  1. Prune stale, recoverable tool output without an inference call.
  2. Summarize older history into a structured checkpoint when necessary.
- Context-overflow recovery and model-aware compaction budgets.
- Token estimation anchored to actual provider usage when available.
- Output-token clamping to remaining context capacity.
- Cross-provider history normalization: thinking blocks, tool-call IDs, missing
  results, empty results, and unsupported images.
- Recovery from thinking blocks invalidated by edited history.
- Provider-supported prompt caching and cache-retention options.
- Input, output, and cache-token accounting.
- Estimated cost calculation, including cache pricing and long-context tiers.

## Project customization

- Standing instructions loaded from `AGENTS.md` or `CLAUDE.md`.
- Resource layers from cupel home, the project's `.cupel/`, and the working
  directory.
- Custom configuration home through `CUPEL_HOME`.
- Markdown prompt templates in `prompts/<name>.md`, exposed as `/name` commands.
- Template descriptions through frontmatter or the first non-empty body line.
- Quoted command arguments.
- Template substitutions: `$1`, `$2`, `$@`, `$ARGUMENTS`, `${N:-default}`,
  `${@:N}`, and `${@:N:L}`.
- More-specific templates override same-name templates from earlier layers.
- Layered home/project settings and presets.
- Hot reload of instructions, templates, models, settings, guardrails, and tools.
- In-place instruction updates appended as compact context diffs rather than
  re-embedding complete files.
- Runtime model, thinking level, preset prompt, and session-entered keys survive
  reloads.
- Project `.cupel/` scaffolding deferred until the first agent interaction.

## Sessions and persistence

- Persistent JSONL transcripts under
  `<cupel home>/sessions/<project-slug>/<session-id>.jsonl`.
- Versioned transcript headers and append-flushed finalized messages.
- Full-history resume in both the agent context and the TUI transcript.
- Resume the newest session or a specific session ID.
- Continued appending to the original transcript when resuming.
- Session listings with IDs, dates, models, message counts, and first-prompt
  labels.
- Fresh-session creation without restarting cupel.
- Resume another session through `/hot-reload <session-id>`.
- Session-ID autocomplete from transcripts on disk.
- Complete on-disk history preserved during context compaction.
- Malformed message lines skipped with warnings during transcript loading;
  incompatible or malformed headers rejected.
- Transcript creation deferred until the first agent interaction.

## Parallel worktree sessions

- Create named Git worktree sessions with `/spinoff <name> [preset]`.
- Dedicated `cupel/spinoff/<name>` branches and project-local worktrees.
- Multiple independent agents running concurrently in one TUI.
- Independent conversation history and working directory for each session.
- Inherit runtime configuration or select a preset for a new spinoff.
- Switch sessions through a sidebar, mouse clicks, or `Ctrl+N` / `Ctrl+P`.
- Per-session running, completed, failed, and new-session indicators.
- Restore existing spinoffs and their newest transcripts after restarting.
- Detect potential merge conflicts between checkout changes after agent runs,
  including uncommitted changes and non-ignored untracked files.
- Conflict notices and sidebar badges without modifying checkout files during
  conflict checks.
- Merge spinoffs back into the origin, optionally specifying a commit message.
- Commit a spinoff's remaining uncommitted work before merging or dropping it.
- Send merge-conflict resolution prompts to the origin's model; repeat the merge
  command to finish after resolution.
- Drop spinoffs while retaining a recoverable commit reference.
- Remove completed worktrees/branches and archive their transcripts.
- Busy-session checks and warnings before quitting with other agents running.

## Terminal UI

- Chronological transcript of prompts, prose, reasoning, tools, errors, notices,
  usage, and compaction summaries.
- Streaming Markdown formatting, including emphasis, code blocks, and tables.
- Tool-result previews, individual expansion by click, and global expansion with
  `Ctrl+T`.
- Tool progress, duration, outcome, and patch-diff displays.
- Unicode-aware multiline input, prompt history, and bracketed paste.
- Newlines through `Alt+Enter` without submitting the prompt.
- Fuzzy `@path` completion, including directory drill-down and quoted paths.
- Slash-command and supported argument autocomplete.
- Mouse-wheel and keyboard scrolling, with the scrolled view pinned while new
  output arrives.
- Visible queued follow-ups and steering messages.
- Queue follow-ups with `Enter` while an agent is running.
- Steer an active run with `Ctrl+Enter` where supported, or `Ctrl+J`.
- Restore queued follow-ups for editing with `Alt+Up`.
- Abort an active run with `Esc` or `Ctrl+C`.
- Copy a selected block or the latest answer through OSC 52 with `Ctrl+O`, where
  supported by the terminal.
- Native terminal selection mode through `Ctrl+Y`.
- Footer showing model, provider, thinking level, session ID, usage, cost, and
  context occupancy.
- Distinguish planning windows, optional maximum context ceilings, and assumed
  discovery limits in the footer.
- Animated working indicators and terminal-state restoration on exit.

## Safety, hooks, and extensibility

### Guardrails

- Bash regex deny rules, including built-in blocking of common `rm -rf` forms.
- Additive global and project deny lists.
- Configurable repeated-tool-call loop killer, opt-in through `loopKiller`
  settings.
- Explicit, persistent project trust for an exact canonical working directory.
- Trust-gated project lifecycle hooks and sensitive model configurations.
- Restricted behavior when trust state is missing, unreadable, or malformed.
- Terminal-control-sequence sanitization before TUI rendering.

### Lifecycle hooks

- Executable hooks for `session-start`, `user-prompt-submit`, `stop`, and
  `session-end`.
- JSON input containing session identity, transcript path, working directory,
  timestamp, and event-specific information.
- Deterministic script ordering and ordered background dispatch.
- Cleared environments, fixed system PATH, captured output, and per-hook timeouts.
- Warn-only hook failures; lifecycle hooks observe events rather than vetoing
  actions.

### Library extension points and observability

- Embeddable Rust libraries for inference, agent execution, and coding-agent
  functionality, independent of the TUI.
- Custom provider registries, tools, and search backends.
- Unified streaming and complete-response inference interfaces.
- Custom message conversion, context transformation, API-key resolution,
  tool-call vetoes, stopping rules, steering, and follow-up hooks.
- Tool progress updates, structured tool details, and early-termination support.
- Opt-in tracing through `RUST_LOG` for requests, agent runs, tools, usage, costs,
  retries, compaction, and timing.
- Plain-mode logs to stderr; interactive logs to a file to avoid corrupting the
  terminal UI.

## Slash commands

| Command | Purpose |
| --- | --- |
| `/help` | List commands and prompt templates. |
| `/new` | Start a fresh session. |
| `/model [id]` | List models or switch model. |
| `/provider [name] [api-key]` | List providers or switch provider, optionally saving a key. |
| `/login openai-codex [device]` | Start subscription login. |
| `/logout [openai-codex]` | List stored logins or remove the Codex login. |
| `/thinking <level>` | Set thinking level. |
| `/preset [name]` | List presets or switch model, thinking level, and prompt together. |
| `/review [path ...]` or `/review --diff` | Build a code-review prompt. |
| `/usage` | Show session token and cost totals. |
| `/session-id` | Show the current ID and this project's sessions. |
| `/hot-reload [session-id]` | Reload configuration in place or resume another session. |
| `/spinoff` | List spinoffs. |
| `/spinoff <name> [preset]` | Create a parallel worktree session. |
| `/spinoff merge <name> [message]` | Merge a spinoff into the origin. |
| `/spinoff drop <name>` | Drop a spinoff and archive its sessions. |
| `/quit` | Quit cupel. |
| `/<template> [arguments]` | Expand a user-defined prompt template. |

Plain mode supports `/help`, `/review`, `/quit`, and prompt templates. Other
built-in slash commands are TUI-only; model and thinking selection remain
available through startup CLI arguments.

## Distribution and maintenance

- Prebuilt macOS and Linux binaries for Intel/x86_64 and ARM64 architectures.
- Shell installer with a cupel home layout.
- Source installation through Cargo.
- Homebrew packaging.
- Development-time catalog generation from `models.dev`, with curated
  compatibility settings and checked-in catalog data.
- CI, release, changelog, and version-update automation.

## Current boundaries

- Retrieval is grep-based; index-backed and semantic retrieval are not
  implemented.
- Persistent agent memory is not implemented; the `memory/` location is reserved
  for future use.
- Guardrails and project trust are not an execution sandbox.
- A review prompt requests report-only behavior; it does not enforce read-only
  tool permissions.
- Spinoffs start from the origin's committed HEAD, not its uncommitted changes.
- Spinoff management belongs to the main checkout and the TUI; launching cupel
  inside a spinoff worktree runs a single session there.
- Do not resume the same transcript simultaneously from multiple processes:
  concurrent appends can interleave.
- The catalog generator is a development tool, not an automatic runtime update.

### Source areas

| Area | Source |
| --- | --- |
| Provider-neutral inference | [`crates/cupel-core/src/`](crates/cupel-core/src/) |
| Agent loop and compaction | [`crates/cupel-agent/src/`](crates/cupel-agent/src/) |
| Coding tools, configuration, sessions, and Git worktrees | [`crates/cupel-coding-agent/src/`](crates/cupel-coding-agent/src/) |
| CLI and terminal UI | [`crates/cupel-tui/src/`](crates/cupel-tui/src/) |
| Installation and release support | [`install.sh`](install.sh), [`packaging/`](packaging/), [`.github/workflows/`](.github/workflows/) |
