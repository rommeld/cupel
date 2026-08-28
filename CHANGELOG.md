# Changelog

Releases up to v0.1.15-beta used patch bumps for feature releases;
from v0.2.0-beta, minor = features, patch = fixes.

## [v0.9.0] - 2026-08-27

- add openrouter API to provider list expand model list
- update README

## [v0.8.1] - 2026-08-24

- add long-context price tiers for GPT-5.6 models

## [v0.8.0] - 2026-08-24

- replace hardcoded model catalog with generated catalog.json

## [v0.7.0] - 2026-08-23

- add Markdown renderer to format assistant output accordingly
- set default thinking to medium
- add loop killer to cupel and manage in project speific or home settings.json
- adjust color themes with a unified theme.rs to manage TUI colors

## [v0.6.0-beta] - 2026-08-18

- define model provider configurations in settings.json   - parse file, ignore unknown fields, does not leak any secrets - independent module   - handle directory and serialize information, manage temp files   - create auto-save and hot-reload functionality for persistent API key
- Removed unnecessary entries to README.md

## [v0.5.4-beta] - 2026-08-02

- remove unused code from codebase to make it more readable

## [v0.5.3-beta] - 2026-07-31

- adjust pricing for GPT-5.6 Terra and GPT-5.6 Luna
- reduce CHANGELOG content

## [v0.5.2-beta] - 2026-07-28

- fix interactive ui test to handle model catalog
- fix catalog tests and adjust model number

## [v0.5.0-beta] - 2026-07-28

- minor model updates to the default catalog

## [v0.4.0-beta] - 2026-07-24

- hot-reload: append the AGENTS.md delta in place instead of rebuilding

## [v0.3.0-beta] - 2026-07-23

- drop the shim's orphaned cupel-coding-agent workspace dependency
- remove the Entire CLI integration
- compaction: free pruning tier before the summarization LLM call

## [v0.2.0-beta] - 2026-07-19

- cupel is now able to do file or diff based reviews with just writing /review

## [v0.1.16-beta] - 2026-07-18

- start the TUI without credentials: warning notice instead of a fatal error

## [v0.1.15-beta] - 2026-07-17

- add /hot-reload: apply .cupel changes to a rebuilt session
- show session ids in the TUI: footer id + /session-id listing
- gitignore: whitelist cupel-coding-agent/tests (carries the guard e2e test)
- add bash denylist guard: block rm -rf via the AgentHooks veto point

## [v0.1.13-beta] - 2026-07-12

- add local model support: models.json catalog layers + ollama discovery

## [v0.1.12-beta] - 2026-07-12

- change model provider in the TUI via /provider slash command
- choose model by leveraging slash command /model via a popup
- add auto-complete for model selection and thinking mode

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
