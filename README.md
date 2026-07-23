# cupel

A cupel is a small vessel for refining precious metal. This project borrows that idea: separate useful code context from repository noise, then feed the refined signal into fast local agent workflows.

`cupel` is a lean Rust coding agent focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. I build it on my former favourit agent [pi](https://pi.dev) (a **MASTERPIECE**).

## Crates definition

### 1. `cupel-core`

The inference crate builds the foundation. It contains a provider-neutral chat-completion abstraction, a built-in model catalog (Anthropic, OpenAI, AWS Bedrock, Fireworks), token/cost tracking, request/response tracing, and retry/backoff logic. Other crates depend on it for all LLM calls.

### 2. `cupel-agent`

Includes the basic agent definition and defines an agent loop primitive. It wires a system prompt, message history, and a set of tool definitions into a loop that repeatedly calls the inference layer, parses model tool calls, executes them, and feeds the results back to the model. It also provides context compaction hooks and the `AgentHooks` extension point for intercepting or overriding tool calls mid-flight.

### 3. `cupel-coding-agent`

Use the `ripgrep` crate as the underlying for the **grep tool**. The crate also includes a simple `cuple CLI` to call functionality from the terminal. `ratatui` is the TUI crate of choice.

It implements the concrete coding-agent experience: a terminal UI, `@file-path` fuzzy file referencing, slash commands (`/new`, `/model`, `/thinking`, `/usage`, `/quit`), prompt templates loaded from `prompts/<name>.md`, project context from `AGENTS.md`/`CLAUDE.md`, and the built-in tools (`read`, `grep`, `write`, `edit`, `bash`).

### 4. `cupel-index`

Placehoalder for code searching

### 5. `cupel-memory`

Placehoalder to manage agent memory

## Install

No Rust required - currently support for macOS
(Intel & Silicon) or Linux (x86_64/aarch64, static musl):

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Usage

Currently supported providers: Anthropic, OpenAI (Responses), AWS Bedrock, and Fireworks - plus any OpenAI-compatible local server (ollama, llama-server; see "Local models" below), e.g.:

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
cupel --resume                                                  # continue this project's newest session
cupel --resume cupel-1720000000000                              # continue a specific session by id
```

Slash commands: `/help` lists everything; built-ins (`/new`, `/model <id>`, `/provider <name> [api-key]`, `/thinking <level>`, `/usage`, `/quit`, `/hot-reload`) are handled locally; markdown files in `prompts/<name>.md` (working directory, its `.cupel/` subdirectory, or `~/.cupel`) become `/name` prompt templates with bash-style `$1`/`$@`/`${@:2}` argument substitution. On a name collision the most specific location wins: working directory > `.cupel/` > `~/.cupel`.

Local models: with `ollama serve` running, every pulled model appears automatically in `--help`, `/model`, and `/provider` (probed at `OLLAMA_HOST` or `http://localhost:11434` with a 500ms budget; silently skipped when ollama is down). With no cloud keys exported, cupel defaults to the first discovered model. Discovered models assume a conservative 4096-token context window (ollama's own default); to raise it, or to add any other OpenAI-compatible endpoint (llama-server, LM Studio, a proxy), define the model in a `models.json` in `~/.cupel/` or `<project>/.cupel/`:

```json
[
  {
    "id": "qwen3:8b",
    "name": "Qwen 3 8B (ollama)",
    "api": "openai-completions",
    "provider": "ollama",
    "baseUrl": "http://localhost:11434/v1",
    "reasoning": false,
    "input": ["text"],
    "cost": { "input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0 },
    "contextWindow": 32768,
    "maxTokens": 8192,
    "compat": { "requiresApiKey": false, "supportsStore": false,
                "supportsDeveloperRole": false, "supportsStrictMode": false,
                "maxTokensField": "max_tokens" }
  }
]
```

(For llama-server, the same entry with `"baseUrl": "http://localhost:8080/v1"` works. `api` must be one of the four registered protocols - unknown ones are warned about and skipped. `requiresApiKey: false` marks a keyless local endpoint.)

Providers: `/provider` lists every provider; `/provider <name>` switches to it (model + matching key together), and `/provider <name> <api-key>` hands over a key when nothing is exported - scoped to this session: the key lives in process memory only, is never persisted or echoed, and wins over the environment variable. Switching models across providers via `/model` re-resolves the key the same way.

Project context: `AGENTS.md` (or `CLAUDE.md`) lives either in  `~/.cupel` or `~/.cupel`.

Sessions management: every conversation is persisted as a JSONL transcript in `~/.cupel/sessions/<project-slug>/<session-id>.jsonl`. The current session id is always visible in the TUI footer, and `/session-id` lists this project's sessions. `cupel --resume` reloads this project's newest session - full history back in context and on screen - and keeps appending to the same file; `cupel --resume <session-id>` picks a specific one. Compaction never rewrites the transcript, so it is always the complete conversation. Don't resume the same session from two terminals at once - appends would interleave.

Hot reload: edits to `~/.cupel` or `<project>/.cupel` (an updated `AGENTS.md`, new prompt templates, models.json changes, bash-deny rules) normally apply on the next launch - `/hot-reload` applies them NOW by rebuilding the agent through the same loader startup uses. Bare `/hot-reload` starts a fresh session (new id, empty history); `/hot-reload <session-id>` reloads the configuration AND resumes that session - its id autocompletes from the transcripts on disk. The current model, thinking level, and any session-entered API keys carry over; the old session is closed cleanly (its `session-end` hook fires).

Hooks: drop an executable into `~/.cupel/hooks/<event>/` (global) or `<project>/.cupel/hooks/<event>/` (per project) and cupel runs it on that event with a JSON payload on stdin: `{"event", "sessionId", "sessionRef" (transcript path), "cwd", "timestamp", "prompt"?}`. Events: `session-start`, `user-prompt-submit`, `stop` (run finished), `session-end`. Hooks observe, never veto: failures and timeouts (60s per hook) are logged and ignored.

Guardrails: bash commands run through a deny list before they execute. `rm -rf` (and its spellings: `-fr`, combined flag groups, behind `sudo` or `&&`) is blocked out of the box; the model receives an error naming the rule instead of the command running. Add your own rules - one regex per line, `#` comments - in `~/.cupel/bash-deny` (global) or `<project>/.cupel/bash-deny` (per project); files EXTEND the defaults (union - deny rules never cancel each other). Matching is deliberately conservative: any line of the command matching anywhere blocks, even inside quotes, because a false positive costs one retry while a false negative costs your files. Invalid patterns are warned about and skipped.

Observability: currently implemented through `RUST_LOG`, e.g. `RUST_LOG=cupel_core=info,cupel_agent=info` (per-request tokens/cost/duration, turns, tool timings, retries, compaction) or `cupel_core=trace` to include request bodies. Logs go to stderr in `--plain` mode and to a temp file (path printed at startup) in the TUI.

