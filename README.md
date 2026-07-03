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

## Usage

```sh
# credentials: first provider found wins (or pick one with --model)
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY / FIREWORKS_API_KEY / AWS credentials

cargo run -p cupel-coding-agent               # TUI in the current directory
cargo run -p cupel-coding-agent -- --help     # flags + built-in model list
```

Supported providers: Anthropic, OpenAI (Responses), AWS Bedrock, and Fireworks, e.g.:

```sh
export FIREWORKS_API_KEY=fw-...
cargo run -p cupel-coding-agent -- --model accounts/fireworks/models/kimi-k2p7-code
```

Options: `--model <id>` picks a model from the built-in catalog, `--thinking off|minimal|low|medium|high|xhigh` sets the reasoning level, `--plain` forces the line-based REPL (used automatically when output is piped).

In the TUI: `enter` sends (while the agent works, it queues a steering message), `alt+enter` newline, `esc` aborts the turn, `ctrl-t` expands tool output, `pgup/pgdn` scrolls, `ctrl-c` quits.
