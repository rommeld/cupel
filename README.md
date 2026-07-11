# cupel

A cupel is a small vessel for refining precious metal. This project borrows that idea: separate useful code context from repository noise, then feed the refined signal into fast local agent workflows.

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. I build it on taking insights from my former favourit agent `pi` (a masterpiece).

## Crates definition

### 1. `cupel-core`

The inference crate builds the foundation.

### 2. `cupel-agent`

Includes the basic agent definition and defines an agent loop primitive.

### 3. `cupel-coding-agent`

Use the `ripgrep` crate as the underlying for the **grep tool**. The crate also includes a simple `cuple CLI` to call functionality from the terminal. `ratatui` is the TUI crate of choice.

## Install

No Rust required - currently support for macOS
(Intel & Silicon) or Linux (x86_64/aarch64, static musl). Everything installs
into one home directory, `~/.cupel` (cargo-style): the binary at
`~/.cupel/bin/cupel` (added to your PATH), global `AGENTS.md` and
`prompts/*.md` beside it:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Usage

Currently supported providers: Anthropic, OpenAI (Responses), AWS Bedrock, and Fireworks, e.g.:

```sh
export FIREWORKS_API_KEY=fw-...
cargo run -p cupel-coding-agent --
```

```sh
# credentials: first provider found wins (or pick one with --model)
export FIREWORKS_API_KEY=fw-...   # or OPENAI_API_KEY / ANTHROPIC_API_KEY / AWS credentials

cupel                                                           # runs agent in current directory
cupel --help                                                    # built-in model list
cupel --model accounts/fireworks/models/kimi-k2p7-code          # select model from list
cupel --model <id> --thinking off|minimal|low|medium|high|xhigh # define thinking mode
```

Slash commands: `/help` lists everything; built-ins (`/new`, `/model <id>`, `/thinking <level>`, `/usage`, `/quit`) are handled locally; markdown files in `prompts/<name>.md` (working directory or `~/.cupel`) become `/name` prompt templates with bash-style `$1`/`$@`/`${@:2}` argument substitution. Typing `/` opens autocomplete.

Project context: an `AGENTS.md` (or `CLAUDE.md`) in the working directory or in `~/.cupel` is loaded into the system prompt on every request. `~/.cupel` is cupel's home (override with `CUPEL_HOME`): the installer puts the binary in `~/.cupel/bin`, global prompt templates in `~/.cupel/prompts/`, and the future memory feature will live in `~/.cupel/memory/`.

Observability: set `RUST_LOG` to enable tracing, e.g. `RUST_LOG=cupel_core=info,cupel_agent=info` (per-request tokens/cost/duration, turns, tool timings, retries, compaction) or `cupel_core=trace` to include request bodies. Logs go to stderr in `--plain` mode and to a temp file (path printed at startup) in the TUI.

## Implementation milestones

### What works today?
- Multi-provider inference layer with build-in model catalog
- CLI: `--model <id>`, `--thinking <mode>`
- Agent tools: read, grep, write, edit, bash
- TUI based on `ratatui`
- File referencing via `@file-path` using fuzzy search
- Context management: proactive compaction + reactive provider truncation
- Auto-retry, tracing/observability, and system-prompt project context

### What is missing?
- Persistencey: sessions will not survive after exiting `cupel`.
- `cupel-index` as an alternative to `grep`(combination of `fff` and `entire`'s code search)
- No local models (e.g. `ollama` support)
- No Windows support
- No MCP integration
