# Changelog

All notable changes to cupel, newest first. On every release the `changelog`
job in `.github/workflows/release.yml` runs `packaging/update-changelog.sh`,
which prepends a section listing the commit subjects since the previous tag
and commits it back to `main` - so commit subjects should read as user-facing
change descriptions. Changes not yet in a release live under `[Unreleased]`;
that section is replaced by the generated one when the next tag ships.

## [Unreleased]

- guardrails: bash commands are checked against a deny list before
  executing - `rm -rf` and variants are blocked by default, and users add
  their own regex rules in `~/.cupel/bash-deny` or `.cupel/bash-deny`
  (union with the defaults; the model gets an error naming the rule)

## [Unreleased]

- settings.json (`~/.cupel` or `.cupel`, project wins per field): default
  model and thinking level (CLI flags still win), plus per-session usage
  limits (maxCostUsd, maxTotalTokens) that stop new prompts once crossed;
  loaded at startup and by /hot-reload
- TUI: `/hot-reload` re-reads every `.cupel` layer (AGENTS.md, prompts,
  models.json, bash-deny) into a rebuilt agent - bare starts a fresh
  session, `/hot-reload <session-id>` resumes that session with the new
  configuration; session ids autocomplete from disk
- TUI: the footer shows the current session id, and `/session-id` lists
  this project's sessions - id, date, message count, model, and the first
  prompt as label (transcripts store no summary, so the opening prompt is
  the label), current session marked with `*`

## [v0.1.13-beta] - 2026-07-12

- add local model support: models.json catalog layers + ollama discovery
- changelog: v0.1.12-beta

## [v0.1.12-beta] - 2026-07-12

- change model provider in the TUI via /provider slash command
- choose model by leveraging slash command /model via a popup
- add auto-complete for model selection and thinking mode
- changelog: v0.1.11-beta

## [v0.1.11-beta] - 2026-07-12

- add selection mode Ctrl+Y to use copy/paste in TUI
- add entire-agent-cupel: Entire CLI external-agent shim (protocol v1)
- cupel got persistency through session transcripts, lifecycle hooks, and session resuming

## [v0.1.10-beta] - 2026-07-12

- fix CI break while creating changelog

## [v0.1.8-beta] - 2026-07-11

- cupel now adds .cupel to root to keep project clean
- update project documentation

## [v0.1.7-beta] - 2026-07-11

- adopt a dedicated cupel home directory (cargo layout): the binary installs
  to `~/.cupel/bin`, global `AGENTS.md` and `prompts/` templates live next to
  it, `memory/` is reserved for the future memory feature; override the
  location with `CUPEL_HOME`

## [v0.1.6-beta] - 2026-07-11

- replace the skills feature with slash commands: markdown files in
  `prompts/<name>.md` become `/name` prompt templates with bash-style
  `$1`/`$@`/`${@:2}` argument substitution; `/` opens autocomplete in the TUI

## [v0.1.5-beta] - 2026-07-10

- fix CI format error

## [v0.1.4-beta] - 2026-07-10

- comment pass across the codebase for easier understanding

## [v0.1.3-beta] - 2026-07-10

- Homebrew packaging (`brew install` via tap) with an automated formula bump
  in the release pipeline
- update README.md to match the current implementation state

## [v0.1.2-beta] - 2026-07-10

- add `@file-path` references to the TUI: fuzzy search over project files and
  inject the selected file into the conversation

## [v0.1.1-beta] - 2026-07-03

- fix bash tool error on Linux distributions

## [v0.1.0-beta] - 2026-07-03

Initial public release: a production-ready coding agent harness.

- multi-provider inference layer with a built-in model catalog: Anthropic
  (incl. Claude Code OAuth), OpenAI Responses API, Amazon Bedrock
  ConverseStream, and Fireworks (with session affinity)
- provider-neutral streaming: shared SSE decoder and reconstruction of tool
  calls from streamed deltas
- agent loop with auto-retry on transient provider errors
- coding tools: read, edit, write, bash, and grep
- context management: proactive auto-compaction (estimate the next request
  size) plus reactive per-provider handling
- eager `AGENTS.md`/`CLAUDE.md` project-context loading into the system prompt
- tracing observability: per-request tokens/cost/duration, turns, tool
  timings, retries, compaction
- ratatui TUI (interactive mode) and `--plain` mode
- release pipeline: universal macOS binary (arm64 + x86_64) and static musl
  Linux binaries (x86_64, aarch64), installable via `install.sh`
