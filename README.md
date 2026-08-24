<p align="center">
  <img src="https://github.com/user-attachments/assets/ade2934b-199b-4838-b38c-d5595685bf85" alt="Projektlogo" width="200">
</p>

# cupel

A cupel is a small vessel for refining precious metals. This project borrows that idea: it separates useful code context from repository noise, then feeds that refined signal into fast, local agent workflows.

`cupel` is a lean Rust coding agent focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. It is heavily inspired by my former favourite agent [pi](https://pi.dev) (a **MASTERPIECE**).

## Workspace crates

### 1. `cupel-core`

The inference crate forms the foundation. It contains a provider-neutral chat-completion abstraction, a built-in model catalog (Anthropic, OpenAI, AWS Bedrock, Fireworks), token/cost tracking, request/response tracing, and retry/backoff logic. Other crates depend on it for all LLM calls.

### 2. `cupel-agent`

Defines the basic agent and its loop primitive. It wires a system prompt, message history, and a set of tool definitions into a loop that repeatedly calls the inference layer, parses model tool calls, executes them, and feeds the results back to the model. It also provides context compaction hooks and the `AgentHooks` extension point for intercepting or overriding tool calls mid-flight.

### 3. `cupel-coding-agent`

Implements the concrete coding-agent experience: a terminal UI, `@file-path` fuzzy file referencing, slash commands (`/help`, `/new`, `/model`, `/provider`, `/thinking`, `/usage`, `/hot-reload`, `/session-id`, `/quit`), prompt templates loaded from `prompts/<name>.md`, project context from `AGENTS.md`/`CLAUDE.md`, and the built-in tools (`read`, `grep`, `write`, `edit`, `bash`). It uses the `ripgrep` crate as the underlying engine for the **grep tool** and `ratatui` for the TUI. The crate also includes a simple `cupel` CLI for calling functionality from the terminal.

## Install

No Rust required. Currently supported: macOS (Intel & Silicon) or Linux (x86_64/aarch64, static musl):

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Usage

Currently supported providers: Anthropic, OpenAI (Responses), AWS Bedrock, and Fireworks — plus any OpenAI-compatible local server (Ollama, `llama-server`; see "Local models" below).

### Project context

`AGENTS.md` (or `CLAUDE.md`) lives either in `~/.cupel` (global) or `<project>/.cupel` (project-specific). On a name collision, the most specific location wins: working directory > `.cupel/` > `~/.cupel`.

### Slash commands

`/help` lists everything. Built-ins (`/new`, `/model <id>`, `/provider <name> [api-key]`, `/thinking <level>`, `/usage`, `/hot-reload`, `/session-id`, `/quit`) are handled locally. Markdown files in `prompts/<name>.md` (working directory, its `.cupel/` subdirectory, or `~/.cupel`) become `/name` prompt templates with bash-style `$1`/`$@`/`${@:2}` argument substitution. On a name collision, the most specific location wins.

### Local models

With `ollama serve` running, every pulled model appears automatically in `--help`, `/model`, and `/provider` (probed at `OLLAMA_HOST` or `http://localhost:11434` with a 500ms budget; silently skipped when Ollama is down). With no cloud keys exported, `cupel` defaults to the first discovered model. Discovered models assume a conservative 4096-token context window (Ollama's own default). To raise it, or to add any other OpenAI-compatible endpoint (`llama-server`, LM Studio, a proxy), define the model in a `models.json` in `~/.cupel/` or `<project>/.cupel/`:

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

For `llama-server`, use the same entry with `"baseUrl": "http://localhost:8080/v1"`. `api` must be one of the four registered protocols — unknown ones are warned about and skipped. `requiresApiKey: false` marks a keyless local endpoint.

### Providers

Built-in providers:

- `anthropic` — Anthropic Messages API
- `openai` — OpenAI Responses API
- `amazon-bedrock` — AWS Bedrock ConverseStream
- `fireworks` — Fireworks OpenAI-compatible completions
- `openrouter` — OpenRouter OpenAI-compatible completions gateway

`/provider` lists every provider. `/provider <name>` switches to it (model + matching key together), and `/provider <name> <api-key>` supplies a key when nothing is exported. The key is scoped to this session: it lives in process memory only, is never persisted or echoed, and takes precedence over the environment variable. Switching models across providers via `/model` re-resolves the key in the same way.

### Session management

Every conversation is persisted as a JSONL transcript in `~/.cupel/sessions/<project-slug>/<session-id>.jsonl`. The current session ID is always visible in the TUI footer, and `/session-id` lists this project's sessions. `cupel --resume` reloads this project's newest session — full history back in context and on screen — and keeps appending to the same file. `cupel --resume <session-id>` picks a specific one.

Compaction never rewrites the transcript, so the transcript remains the complete conversation. Do not resume the same session from two terminals at once — appended entries would interleave.

### Hot reload

Edits to `~/.cupel` or `<project>/.cupel` (an updated `AGENTS.md`, new prompt templates, `models.json` changes, bash-deny rules) normally apply on the next launch. `/hot-reload` applies them immediately.

Bare `/hot-reload` updates the running session in place: same ID, same history, same transcript file. Fresh templates, models, bash-deny rules, and tools are swapped in, and `AGENTS.md` changes are appended as a compact unified diff message (`[context update]`) instead of re-embedding the whole file. The original stays in the system prompt from session start, so only the changed instructions cost tokens.

`/hot-reload <session-id>` resumes another session with a full rebuild (fresh system prompt included). Session IDs autocomplete from the transcripts on disk.

The current model, thinking level, and session-entered API keys carry over in both modes. Only the resume mode closes the old session (`session-end` hook fires); an in-place reload is not a session boundary.

## Hooks and guardrails

### Hooks

Drop executables into `~/.cupel/hooks/<event>/` (global) or `<project>/.cupel/hooks/<event>/` (per project). `cupel` runs them on that event with a JSON payload on stdin. The payload schema is:

```json
{
  "event": "session-start",
  "sessionId": "...",
  "sessionRef": "...",
  "cwd": "...",
  "timestamp": "...",
  "prompt": "..."
}
```

`sessionRef` is the transcript path; `prompt` is present for `user-prompt-submit` and absent otherwise.

Events: `session-start`, `user-prompt-submit`, `stop` (run finished), `session-end`.

Hooks observe but never veto. Failures and timeouts (60s per hook) are logged and do not block execution.

### Guardrails

Bash commands are checked against a deny list before they execute. `rm -rf` (and its spellings: `-fr`, combined flag groups, behind `sudo` or `&&`) is blocked out of the box. The model receives an error naming the rule instead of the command being executed.

Add your own rules — one regex per line, `#` comments — in `~/.cupel/bash-deny` (global) or `<project>/.cupel/bash-deny` (per project).
A `loopKiller` setting also blocks repeated identical tool calls after `maxRepeats` consecutive attempts, redirecting the model to a different approach.

```json
{
  "loopKiller": {
    "maxRepeats": 3
  }
}
```
