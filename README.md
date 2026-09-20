<p align="center">
  <img src="https://github.com/user-attachments/assets/ade2934b-199b-4838-b38c-d5595685bf85" alt="Projektlogo" width="200">
</p>

# cupel

A cupel is a small vessel for refining precious metals. This project borrows that idea: it separates useful code context from repository noise, then feeds that refined signal into fast, local agent workflows.

`cupel` is a lean Rust coding agent focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. It is heavily inspired by my former favourite agent [pi](https://pi.dev) (a **MASTERPIECE**).

## Workspace crates

- **`cupel-core`** — provider-neutral chat-completion abstraction with a built-in model catalog, token/cost tracking, request/response tracing, and retry/backoff. The foundation for all LLM calls.
- **`cupel-agent`** — the agent loop: wires system prompt, message history, and tool definitions into repeated inference calls, executes tool calls, and feeds the results back. Includes context-compaction hooks and the `AgentHooks` extension point.
- **`cupel-coding-agent`** — the coding-agent experience: a `ratatui` TUI, `@file-path` fuzzy referencing, slash commands, prompt templates from `prompts/<name>.md`, project context from `AGENTS.md`/`CLAUDE.md`, and the built-in tools `read`, `grep` (backed by the `grep` crate family), `apply_patch`, and `bash`. Ships the `cupel` CLI.

## Install

No Rust required. macOS (Intel & Silicon) and Linux (x86_64/aarch64, static musl):

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

## Usage

Supported providers: Anthropic, OpenAI (Responses), OpenAI (Codex), AWS Bedrock, Fireworks, and OpenRouter — plus any OpenAI-compatible local server (`ollama`, `llama-server`; see "Local models").

### Project context

`AGENTS.md` (or `CLAUDE.md`) lives in `~/.cupel` (global) or `<project>/.cupel` (per project). On a name collision, the most specific location wins: working directory > `.cupel/` > `~/.cupel`.

### Agent tools

#### apply_patch

One tool for every file change: the model sends a single `*** Begin Patch` / `*** End Patch` envelope that creates, deletes, renames, or patches any number of files. Every hunk is matched against the file first (exact, then ignoring trailing whitespace, then with typographic punctuation folded); if any hunk fails, nothing is written.

#### Grep & Grep Rank

| Metric | Baseline (rg, published) | swe-grep (published) | pgr | cupel |
| ------ | ------------------------ | -------------------- | --- | ----- |
| MRR | 0,318 | 0,306 | 0,405 | 0,455 |
| Hit@1 | 26,0% | 18,0% | 34,0% | 40,0% |
| Hit@3 | 34,0% | 42,0% | 42,0% | 50,0% |
| Hit@5 | — | — | 52,0% | 54,0% |
| Output (tokens) | 6566 | 1427 | 1587 | 64 |

### Slash commands

`/help` lists everything. Built-ins: `/new`, `/model <id>`, `/provider <name> [api-key]`, `/thinking <level>`, `/review [path...]`, `/usage`, `/hot-reload`, `/session-id`, `/quit`. `/review` bundles the project, specific paths, or a `--diff` into a code-review prompt. Markdown files in `prompts/<name>.md` (working directory, its `.cupel/`, or `~/.cupel`) become `/name` prompt templates with bash-style `$1`/`$@`/`${@:2}` substitution; on a name collision, the most specific location wins.

### Local models

With `ollama serve` running, every pulled model appears automatically in `--help`, `/model`, and `/provider` (probed at `OLLAMA_HOST` or `http://localhost:11434` with a 500 ms budget; silently skipped when down). With no cloud keys exported, `cupel` defaults to the first discovered model. Discovered models assume a conservative 4096-token context window. To raise it — or to add any other OpenAI-compatible endpoint (`llama-server`, LM Studio, a proxy) — define the model in a `models.json` in `~/.cupel/` or `<project>/.cupel/`:

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
- `openai-codex` — OpenAI Codex login
- `amazon-bedrock` — AWS Bedrock ConverseStream
- `fireworks` — Fireworks OpenAI-compatible completions
- `openrouter` — OpenRouter completions gateway

`/provider` lists every provider, `/provider <name>` switches to it (model and matching key together), and `/provider <name> <api-key>` supplies a key when nothing is exported. Keys live in session memory and are saved to `~/.cupel/settings.json` (atomic write, owner-only permissions); they are never echoed. Resolution order: session key > environment variable > `~/.cupel/settings.json`. Switching models across providers via `/model` re-resolves the key the same way.

### Session management

Every conversation is persisted as a JSONL transcript in `~/.cupel/sessions/<project-slug>/<session-id>.jsonl`. The current session ID shows in the TUI footer; `/session-id` lists this project's sessions. `cupel --resume` reloads the newest session — full history back in context and on screen — and keeps appending to the same file; `cupel --resume <session-id>` picks a specific one.

Compaction never rewrites the transcript, so it remains the complete conversation. Do not resume the same session from two terminals at once — appended entries would interleave.

### Hot reload

Changes to `~/.cupel` or `<project>/.cupel` (`AGENTS.md`, prompt templates, `models.json`, bash-deny rules) apply on the next launch; `/hot-reload` applies them immediately.

Bare `/hot-reload` updates the running session in place (same ID, history, and transcript): fresh templates, models, deny rules, and tools are swapped in, and `AGENTS.md` changes are appended as a compact `[context update]` diff instead of re-embedding the whole file — only the changed instructions cost tokens. `/hot-reload <session-id>` resumes another session with a full rebuild (fresh system prompt included); session IDs autocomplete from disk.

Model, thinking level, and session-entered keys carry over in both modes. Only the resume mode closes the old session (`session-end` hook fires).

## Hooks and guardrails

### Hooks

Drop executables into `~/.cupel/hooks/<event>/` (global) or `<project>/.cupel/hooks/<event>/` (per project). `cupel` runs them on that event with a JSON payload on stdin:

```json
{
  "event": "session-start",
  "sessionId": "...",
  "sessionRef": "...",
  "cwd": "...",
  "timestamp": 1700000000000,
  "prompt": "..."
}
```

`sessionRef` is the transcript path; `prompt` is present only for `user-prompt-submit`.

Events: `session-start`, `user-prompt-submit`, `stop`, `session-end`. Hooks observe but never veto; failures and timeouts (60 s per hook) are logged, not blocking.

### Guardrails

Bash commands are checked against a deny list before they execute. `rm -rf` (and its spellings: `-fr`, combined flags, behind `sudo` or `&&`) is blocked out of the box; the model receives an error naming the rule. Add your own rules — one regex per line, `#` comments — in `~/.cupel/bash-deny` (global) or `<project>/.cupel/bash-deny` (per project).

A `loopKiller` setting blocks repeated identical tool calls after `maxRepeats` consecutive attempts, redirecting the model to a different approach. Configure it in `~/.cupel/settings.json` (global) or `<project>/.cupel/settings.json` (project overrides home):

```json
{
  "loopKiller": {
    "maxRepeats": 3
  }
}
```
