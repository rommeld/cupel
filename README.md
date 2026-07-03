# cupel

A cupel is a small vessel for refining precious metal. This project borrows that idea: separate useful code context from repository noise, then feed the refined signal into fast local agent workflows.

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. Search and reranking will lean on [fff](https://github.com/dmtrKovalenko/fff), with parts of the architecture inspired by [pi.dev](https://pi.dev) and parts shaped around my own coding workflows.

## Crates definition

### 1. `cupel-core`

The inference crate builds the foundation.

### 2. `cupel-agent`

Includes the basic agent definition and defines an agent loop primitive.

### 3. `cupel-coding-agent`

Use the `ripgrep` crate as the underlying for the **grep tool**. The crate also includes a simple `cuple CLI` to call functionality from the terminal. `ratatui` is the TUI crate of choice.

## Install

No Rust required - the installer downloads a prebuilt binary for macOS
(universal) or Linux (x86_64/aarch64, static musl) and puts `cupel` on your PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Alternatives: download an archive from the [releases page](https://github.com/rommeld/cupel/releases),
`brew install rommeld/tap/cupel` (once the tap is published - see `packaging/README.md`),
or build from source with `cargo build --release -p cupel-coding-agent`.
Windows is not supported yet (the bash tool is Unix-only).

## Usage

```sh
# credentials: first provider found wins (or pick one with --model)
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / FIREWORKS_API_KEY / AWS credentials

cupel            # TUI in the current directory
cupel --help     # flags + built-in model list
```

Supported providers: Anthropic, OpenAI (Responses), AWS Bedrock, and Fireworks, e.g.:

```sh
export FIREWORKS_API_KEY=fw-...
cargo run -p cupel-coding-agent -- --model accounts/fireworks/models/kimi-k2p7-code
```

Options: `--model <id>` picks a model from the built-in catalog, `--thinking off|minimal|low|medium|high|xhigh` sets the reasoning level, `--plain` forces the line-based REPL (used automatically when output is piped).

In the TUI: `enter` sends (while the agent works, it queues a steering message), `alt+enter` newline, `esc` aborts the turn, `ctrl-t` expands tool output, `pgup/pgdn` scrolls, `ctrl-c` quits.

Project context: an `AGENTS.md` (or `CLAUDE.md`) in the working directory or next to the installed binary is loaded into the system prompt on every request. Skills are discovered under `skills/<name>/SKILL.md` in the same two locations; only their name/description enter the prompt, and the agent reads the full skill file on demand when a task matches.

Observability: set `RUST_LOG` to enable tracing, e.g. `RUST_LOG=cupel_core=info,cupel_agent=info` (per-request tokens/cost/duration, turns, tool timings, retries, compaction) or `cupel_core=trace` to include request bodies. Logs go to stderr in `--plain` mode and to a temp file (path printed at startup) in the TUI.

## Implementation milestones

- Persistencey is currently missing from the project. Sessions will not survive after exiting `cupel`.
- Alternative to `grep` will be implemented in `cupel-index` using a combination of `fff` and `entire`'s code search.
- Expand existing model providers (e.g., integrate local models via `ollama`).
