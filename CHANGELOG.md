# Changelog

Notable user-facing changes by release.

## [Unreleased]

- TUI: Highlight Reasoning headlines in green without changing the body text. Customize the headline with `palette.reasoning_header` (a `#rrggbb` color) in your theme JSON.

## [v0.50.1] - 2026-11-20

- Add 10 OpenRouter models, including Gemini 4 Neon and GPT-6.2 Sol and Astra (standard and Pro). Fireworks and provider defaults are unchanged.

## [v0.50.0] - 2026-11-20

- On Unix, start background Bash commands and use the `jobs` tool to check their exit status, read recent output, or stop them. Jobs are tied to the session and stop on reload or exit.
- While jobs are running, Cupel blocks conflicting undo and spinoff operations and prevents another ACP session from prompting in the same directory.

## [v0.40.0] - 2026-11-20

- Add on-demand skills from `$CUPEL_HOME/skills/<skill>/SKILL.md` and `.cupel/skills/<skill>/SKILL.md`. Project skills override home skills with the same name. The agent loads instructions only when it uses a skill.
- TUI: Press `Esc` twice within 500 ms to clear an idle, unsent prompt without changing prompt history.
- TUI and plain mode: Use `/clear` between turns to clear displayed output without resetting the session or dropping conversation history.
- TUI: Wrap Markdown table cells without truncation and show rows as labeled records when the pane is too narrow for columns.
- Add Gemini 4 Neon to Google with `xhigh` thinking and long decode within its 1M-token window.

## [v0.39.0] - 2026-11-17

- Add GPT-6.2 Sol and Astra to OpenAI and OpenAI Codex with flat full-window pricing, plus ultrafast variants for the $500 subscription. Cost estimates use standard rates where ultrafast prices are unpublished.

## [v0.38.2] - 2026-11-16

- Add Qwen4 Max on OpenRouter with image input and adjustable reasoning effort.

## [v0.38.1] - 2026-11-15

- Update `quinn-udp` to 0.5.16 to resolve RUSTSEC-2026-0538.
- Update AWS SDK dependencies to remove unmaintained `pin-utils` (RUSTSEC-2026-0441).

## [v0.38.0] - 2026-11-13

- Support checkpointed Gemini 4 long decode with per-request usage pricing. Add Gemini 4 Hydrogen to Google and OpenRouter with `xhigh` thinking and provider-specific output limits.
- Expand Bedrock, Fireworks, and OpenRouter model coverage.
- Use retained context rather than billed usage for compaction, output budgets, and the TUI meter. Exclude failed turns from context estimates and summaries.
- Do not retry automatically once streamed output can no longer be replayed safely.
- Update `rustix` to 1.1.6 to resolve the extended-attribute unsoundness advisory.

## [v0.37.0] - 2026-11-11

- Add native Gemini streaming support, Gemini models, and Google API key setup.
- Improve patch safety with stale-source checks and reject aliased patch destinations.

## [v0.36.0] - 2026-11-09

- Fix how the TUI displays completed reasoning and preserve structured OpenRouter reasoning on replay.
- TUI: Show total agent working time, let users cycle tool output visibility without hiding diffs, and add draggable scrollbars to the prompt and usage views.
- Add Claude Fable 5.2 on Anthropic and OpenRouter; retire GPT-6 Sol from OpenAI and Codex.
- Update the ACP adapter to protocol SDK 5.

## [v0.35.2] - 2026-11-07

- TUI: Scroll with the mouse wheel while drag-selecting the transcript, and use `Ctrl+O` to copy without switching modes. Improve rendering of fenced code blocks.
- Fix undo conflict detection. Formatter and external edits no longer block valid patches; Cupel warns and disables undo when restoring changes would be unsafe.
- Undo history now expires when a session closes. Transcripts are preserved.
- Remove Homebrew installation support. Use `install.sh` or the TUI's `/update` instead.

## [v0.35.1] - 2026-11-06

- TUI: Use `Ctrl+Left`/`Right` or `Alt+Left`/`Right` to move by word, move vertically across wrapped lines, or click to position the prompt cursor.
- TUI: Use `Shift+Up` and `Shift+Down` to recall prompt history without losing the current draft.

## [v0.35.0] - 2026-11-06

- Add `/changes` and per-turn `/undo` in TUI and plain mode. Undo preserves existing user edits and refuses to overwrite newer file changes. Snapshots can survive resume and hot reload.
- Ask the agent to acknowledge mistakes directly and skip routine status updates.
- TUI: Give user prompts in the transcript padding and a distinct, theme-aware background.

## [v0.34.13] - 2026-11-06

- TUI: Click or drag the transcript scrollbar with mouse capture enabled. Large token counts use `M`, `B`, and `T` suffixes.
- TUI: Use `Shift+Enter` for newlines and `Ctrl+Enter` for steering.
- Remove the built-in `/review` command.

## [v0.34.12] - 2026-11-06

- TUI: Reasoning blocks are expanded by default. Click a block's header to toggle it, or press `Ctrl+R` to toggle all blocks.

## [v0.34.11] - 2026-11-06

- Remove helper agents, teams, delegation, and their settings and controls. Single-agent parallel tool execution remains supported.
- Refresh model metadata and expand provider coverage.
- TUI: Use compact token counts and show costs to three decimal places in the usage popup.
- Add `/exit` as an alias for `/quit`.

## [v0.34.10] - 2026-11-05

- Find and resume saved sessions by opening prompt rather than internal ID with `/sessions`, `/hot-reload`, and `--resume`.

## [v0.34.9] - 2026-11-04

- Add Particle-1 and GLM 5.3 FlashX Legal for OpenRouter, remove Laguna S 2.1, and refresh pricing, token limits, and reasoning options.

## [v0.34.8] - 2026-11-04

- Start collaboration services only when needed and preserve team history and usage when returning to solo mode.

## [v0.34.7] - 2026-11-01

- TUI: Use `Ctrl+Enter` for prompt newlines and `Alt+Enter` for agent steering.

## [v0.34.6] - 2026-10-31

- Show file paths, line ranges, search arguments, commands, and patch summaries for helper tool calls.

## [v0.34.5] - 2026-10-31

- Keep final answers below live team updates, identify helpers and models in the transcript, and toggle team updates with `Ctrl+B`.

## [v0.34.4] - 2026-10-31

- Add an implementer team agent and Developer-to-Reviewer handoffs. Allow standalone Markdown review reports when requested.
- Add example TUI themes.

## [v0.34.3] - 2026-10-30

- Limit what helpers share and keep background updates out of active tool exchanges.
- Preserve complete tool schemas in Anthropic requests.

## [v0.34.0] - 2026-10-28

- Configure helper agents and command mappings in trusted projects. Use `/agent` to call a helper or opt into a session team.
- Teams support delegation with shared budgets and cancellation, persistent discussions and private histories, changed-file reports, and per-agent spend.
- Preserve streamed answers when team updates arrive.

## [v0.33.13] - 2026-10-27

- Make GPT-6.1 Sol the default for OpenAI and Codex, and expand the Fireworks and OpenRouter catalogs.

## [v0.33.12] - 2026-10-27

- Add GPT-6.1 Terra for OpenAI and Codex.

## [v0.33.11] - 2026-10-27

- Correct the OpenRouter credential hint.

## [v0.33.10] - 2026-10-27

- Report agent status to terminals via OSC 7501.
- Support Anthropic and Bedrock gateways.

## [v0.33.8] - 2026-10-25

- Fix macOS universal release builds.

## [v0.33.7] - 2026-10-25

- Bundle `cupel-acp` in release archives and install it alongside `cupel` with `install.sh`. The TUI's `/update` still updates only `cupel`.

## [v0.33.6] - 2026-10-25

- Add a persistent ACP frontend over stdio, with examples for OpenHands and Zed.

## [v0.33.5] - 2026-10-24

- Aggregate model usage from saved sessions.

## [v0.33.4] - 2026-10-23

- Add Kimi K3.1 Pro on OpenRouter.

## [v0.33.3] - 2026-10-22

- Remove older GPT-5.x and Claude Sonnet 4.x entries from the built-in catalog.

## [v0.33.2] - 2026-10-22

- Add MiniMax M3.1 Flash on OpenRouter and DeepSeek V4.1 Flash on Fireworks.
- TUI: Show the model and reasoning effort above the prompt instead of in the footer.
- Update dependencies to fix TLS certificate handling.

## [v0.33.1] - 2026-10-21

- TUI: Give open questions a dedicated decision pane.

## [v0.33.0] - 2026-10-18

- Let the agent ask design questions and collect the user's answer above the prompt.
- Preserve tool results that cannot be reproduced during context compaction.

## [v0.32.1] - 2026-10-18

- Update DeepSeek V4.1 Flash on OpenRouter to the 1017 snapshot.

## [v0.32.0] - 2026-10-18

- TUI: Notify users once per launch when a newer release is available. Use `/update` to replace a binary installed with `install.sh` and restart into the new version.

## [v0.31.1] - 2026-10-17

- TUI: Load custom themes from JSON files.

## [v0.31.0] - 2026-10-16

- TUI: Highlight Markdown with colors from the active theme.
- Plan context for `codex/gpt-6.1-sol` using Codex's default 500k-token window.

## [v0.30.0] - 2026-10-16

- TUI: Switch color themes and keep the selected theme across sessions.

## [v0.29.3] - 2026-10-14

- Report the correct version in release binaries.
- Reject invalid session IDs passed to `--resume`.
- TUI: Remove titles and side borders from the prompt and transcript.

## [v0.29.2] - 2026-10-13

- Add Muse Wonder on OpenRouter.

## [v0.29.1] - 2026-10-13

- Add Gemini 4 Argon on OpenRouter and GPT-6.1 Sol Ultrafast on OpenAI and Codex.
- Use the lowest reasoning effort when `off` is selected for a model that cannot disable reasoning.
- TUI: Show agent activity, elapsed time, reasoning token counts, and the current version. Render reasoning as Markdown.

## [v0.27.0] - 2026-10-10

- Steer the agent while a run is active.
- Make Codex token refresh more reliable: adopt rotated tokens, retain valid tokens on refresh failures, and report rejected logins.
- Isolate invalid provider and preset settings, reject an invalid default preset, and warn about invalid model compatibility settings.
- Improve retry and context-overflow handling. Estimate context from resumed history until fresh usage is available.
- Redact API keys from `StreamOptions` debug output.

## [v0.26.0] - 2026-10-08

- Add GPT-6.1 Sol on OpenRouter and expand the catalog with MiMo V2.6, Ember-1, and Fireworks Inkling. Replace Anthropic Haiku 4.5 with Haiku 5.5.

## [v0.25.0] - 2026-10-06

- Use `/spinoff` to run parallel sessions in Git worktrees. Spinoffs survive restarts, warn about conflicts, and can be merged or dropped.
- Clean up plain-mode runs when the output pipe closes.
- TUI: Distinguish the planning context limit from the assumed context limit.

## [v0.24.0] - 2026-10-03

- Explain content-filter stops and preserve partial output and usage. Treat incomplete Anthropic and Bedrock streams as retryable errors.
- Reset usage limits after pruning or compaction and use the actual model window to set Ollama compaction thresholds.

## [v0.23.0] - 2026-10-02

- Require project trust before enabling project hooks. Sanitize transcript text before rendering.
- Honor the `off` reasoning setting on Bedrock and stop retrying permanent HTTP 4xx errors indefinitely.
- Make `/new` close the current session before starting another.
- Fix `apply_patch` when a patch edits and renames a file. Defer compaction until the assistant can account for the latest tool result.

## [v0.22.0] - 2026-10-01

- Build and install the `cupel` binary from `cupel-tui`; `cargo run` still launches it.

## [v0.21.2] - 2026-09-29

- Add GPT-6.1 Sol to the OpenAI catalog.

## [v0.21.1] - 2026-09-29

- Add Claude Fable 5.1 on Anthropic and Bedrock.

## [v0.21.0] - 2026-09-29

- Add Claude Sonnet 5.5 on Anthropic and Bedrock. Stop capping `xhigh` and `max` on Bedrock.
- Discard reasoning blocks invalidated by history edits.
- Replace retired Fireworks models.

## [v0.20.0] - 2026-09-27

- Queue follow-up prompts while the agent works.

## [v0.19.0] - 2026-09-27

- Configure model presets in `settings.json`.
- Retry transport failures and report their cause.

## [v0.18.0] - 2026-09-26

- Plain mode: Read piped input as one prompt, reject TUI-only commands, and exit with an error if the model request fails unrecoverably.
- Run shell commands in Bash rather than the login shell, return when Bash exits, and clean up commands on plain-mode termination.
- Prefer Codex login, adjust the default reasoning effort for Sol, and remove unsupported Codex model entries.

## [v0.17.0] - 2026-09-24

- Enable adaptive thinking for Claude Sonnet 5 and Opus 5, and add Opus 5.5 with adaptive thinking.
- Add GPT-6 Sol and Luna on OpenAI, OpenRouter, and Codex, and refresh OpenRouter pricing and output limits.

## [v0.16.0] - 2026-09-20

- Replace `write` with `apply_patch`, which validates a patch before creating, deleting, renaming, or editing files.

## [v0.15.2] - 2026-09-16

- Add GLM 5.3, GLM 5.3 Flash, Qwen 3.8 Max, and DeepSeek V4.1 Flash on Fireworks.
- Add Kimi K3, GLM 5.3 Flash, DeepSeek V4 snapshots, Muse Spark 1.3, Inkling, and Laguna S 2.1 on OpenRouter.

## [v0.15.0] - 2026-09-14

- TUI: Render the transcript in a single column.

## [v0.14.0] - 2026-09-13

- Rank `grep` results by relevance.

## [v0.13.0] - 2026-09-12

- Show the agent's coding activity in the TUI.

## [v0.12.0] - 2026-09-06

- Add GPT-6 Astra on OpenAI, Codex, and OpenRouter. Support `max` reasoning where available and honor model-specific context limits and temperature support.

## [v0.11.0] - 2026-09-01

- Add OpenAI Codex as a provider for subscription users, with automatic credential refresh.
- Add `/login` and `/logout`, with browser sign-in and device-code support.

## [v0.10.1] - 2026-08-29

- Keep the transcript position when the user scrolls up during streaming.

## [v0.10.0] - 2026-08-29

- TUI: Give the conversation and tool activity separate panes.

## [v0.9.0] - 2026-08-27

- Add OpenRouter support and its model catalog.

## [v0.8.1] - 2026-08-24

- Account for long-context pricing tiers when estimating cost for GPT-5.6 models.

## [v0.8.0] - 2026-08-24

- Bundle a model catalog generated from models.dev without fetching it at runtime.

## [v0.7.0] - 2026-08-23

- Render assistant output as Markdown and set default reasoning effort to `medium`.
- Configure loop prevention in project or home `settings.json`.

## [v0.6.0-beta] - 2026-08-18

- Configure model providers and persistent API keys in `settings.json`, with hot reload.

## [v0.5.3-beta] - 2026-07-31

- Update prices for GPT-5.6 Terra and Luna.

## [v0.5.0-beta] - 2026-07-28

- Add Claude Sonnet 5, Claude Opus 5, and OpenAI GPT-5.6 Solar, Luna, and Terra to the built-in catalog.

## [v0.4.0-beta] - 2026-07-24

- Apply changes to `AGENTS.md` on hot reload without rebuilding the session.

## [v0.3.0-beta] - 2026-07-23

- Remove the Entire CLI integration.
- Prune context before requesting a model-generated summary.

## [v0.2.0-beta] - 2026-07-19

- Add `/review` for reviewing files and diffs.

## [v0.1.16-beta] - 2026-07-18

- Warn about missing credentials instead of failing to start the TUI.

## [v0.1.15-beta] - 2026-07-17

- Add `/hot-reload` to apply `.cupel` changes and `/session-id` to list sessions.
- Block `rm -rf` in the Bash tool with a denylist guard.

## [v0.1.13-beta] - 2026-07-12

- Support local models through `models.json` and Ollama discovery.

## [v0.1.12-beta] - 2026-07-12

- TUI: Switch providers with `/provider` and choose models and reasoning effort with autocomplete.

## [v0.1.11-beta] - 2026-07-12

- Save session transcripts and resume sessions. Add Entire CLI integration and lifecycle hooks.
- TUI: Toggle selection mode with `Ctrl+Y` for copying and pasting.

## [v0.1.8-beta] - 2026-07-11

- Keep project-specific files in a `.cupel` directory at the project root.

## [v0.1.7-beta] - 2026-07-11

- Use `~/.cupel` for the binary (`bin/`), global `AGENTS.md`, and prompt templates. Override the location with `CUPEL_HOME`.

## [v0.1.6-beta] - 2026-07-11

- Replace the original skills feature with slash commands. Files in `prompts/<name>.md` become `/name` templates with Bash-style argument substitution and TUI autocomplete.

## [v0.1.3-beta] - 2026-07-10

- Add Homebrew installation via a tap.

## [v0.1.2-beta] - 2026-07-10

- TUI: Use `@file-path` to find project files and add them to the conversation.

## [v0.1.1-beta] - 2026-07-03

- Fix a Bash tool error on Linux.

## [v0.1.0-beta] - 2026-07-03

Initial public release.

- Interactive TUI and `--plain` mode with `read`, `edit`, `write`, `bash`, and `grep` tools.
- Streaming inference and a built-in model catalog for Anthropic (including Claude Code OAuth), OpenAI, Amazon Bedrock, and Fireworks.
- Automatic retries and context compaction, project instructions from `AGENTS.md` and `CLAUDE.md`, and tracing for token usage, cost, and timings.
- Universal macOS and static Linux binaries installable with `install.sh`.
