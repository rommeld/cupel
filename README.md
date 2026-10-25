<p align="center">
  <img width="300" height="330" alt="image" src="https://github.com/user-attachments/assets/a4174b06-a4b7-438e-bbcb-4a38ed98bd4f" />
</p>

# cupel

A cupel is a small vessel for refining precious metals. This project borrows that idea: it separates useful code context from repository noise, then feeds that refined signal into fast, local agent workflows.

`cupel` is a lean Rust coding agent focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval.

## Contents

- [Workspace crates](#workspace-crates)
- [Install](#install)
  - [Prebuilt binary](#prebuilt-binary)
  - [From source](#from-source)
- [Usage](#usage)
  - [Project context](#project-context)
  - [Slash commands](#slash-commands)
  - [Providers](#providers)
  - [Themes](#themes)
  - [Local models](#local-models)
  - [Guardrails](#guardrails)
- [ACP clients](#acp-clients)
  - [Zed IDE](#zed-ide)
  - [OpenHands](#openhands)
- [Appendix](#appendix)

## Workspace crates

- **`cupel-core`** — provider-neutral chat-completion abstraction with a built-in model catalog, token/cost tracking, request/response tracing, and retry/backoff. The foundation for all LLM calls.
- **`cupel-agent`** — the agent loop: wires system prompt, message history, and tool definitions into repeated inference calls, executes tool calls, and feeds the results back. Includes context-compaction hooks and the `AgentHooks` extension point.
- **`cupel-coding-agent`** — the coding-agent library: slash commands, prompt templates from `prompts/<name>.md`, project context from `AGENTS.md`/`CLAUDE.md`, sessions, the line-based plain mode, and the built-in tools `read`, `grep` (backed by the `grep` crate family), `apply_patch`, and `bash`. It has no UI dependency, so other programs can embed it.
- **`cupel-tui`** — the `ratatui` TUI with `@file-path` fuzzy referencing, built on `cupel-coding-agent`. Ships the `cupel` CLI.
- **`cupel-acp`** — ACP v1 over stdio for OpenHands and other ACP clients; text prompts, tool activity, cancellation, session restoration, and usage updates.

## Install

### Prebuilt binary

No Rust required. macOS (Intel & Silicon) and Linux (x86_64/aarch64, static musl) via `curl`:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

### From source

Requires a recent stable Rust toolchain:

```sh
git clone https://github.com/rommeld/cupel.git
cd cupel
cargo install --path crates/cupel-tui
```

## Usage

Supported providers: Anthropic, OpenAI (Responses), OpenAI (Codex), AWS Bedrock, Fireworks, and OpenRouter — plus any OpenAI-compatible local server (`ollama`, `llama-server`; see "Local models").

### Project context

`AGENTS.md` (or `CLAUDE.md`) lives in `~/.cupel` (global) or `<project>/.cupel` (per project). On a name collision, the most specific location wins: working directory > `.cupel/` > `~/.cupel`.

### Slash commands

| Command | Description |
| ------- | ----------- |
| `/help` | List all commands and prompt templates |
| `/new` | Clear the conversation and start a fresh session |
| `/model <id>` | Switch model (no argument lists them) |
| `/provider <name> [api-key]` | Switch provider, optionally supplying a key (no argument lists them) |
| `/login openai-codex [device]` | Log in with a ChatGPT Plus/Pro subscription |
| `/logout openai-codex` | Remove a stored login |
| `/thinking <level>` | Set thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/preset <name>`| Switch model, thinking level, and prompt to a preset (no argument lists them) |
| `/themes [id]` | Switch the color theme (no argument lists them) |
| `/review [path...]` | Bundle the project, specific paths, or a `--diff` into a code-review prompt |
| `/usage` | Open current-session and all-history token/cost tables by model; close with `Esc` or the top-left `X` |
| `/session-id` | Show the current session ID and list this project's sessions |
| `/hot-reload [session-id]` | Reload `.cupel` config into the running or another session |
| `/quit` | Quit cupel |

Markdown files in `prompts/<name>.md` (working directory, its `.cupel/`, or `~/.cupel`) become `/name` prompt templates with bash-style `$1`/`$@`/`${@:2}` substitution; on a name collision, the most specific location wins.

#### Sessions

Every conversation is persisted as a JSONL transcript in `~/.cupel/sessions/<project-slug>/<session-id>.jsonl`. The current session ID shows in the TUI footer; `/session-id` lists this project's sessions. `cupel --resume` reloads the newest session — full history back in context and on screen — and keeps appending to the same file; `cupel --resume <session-id>` picks a specific one.

Compaction never rewrites the transcript, so it remains the complete conversation. Do not resume the same session from two terminals at once — appended entries would interleave.

#### Hot reload

Changes to `~/.cupel` or `<project>/.cupel` (`AGENTS.md`, prompt templates, `models.json`, bash-deny rules) apply on the next launch; `/hot-reload` applies them immediately.

Bare `/hot-reload` updates the running session in place (same ID, history, and transcript): fresh templates, models, deny rules, and tools are swapped in, and `AGENTS.md` changes are appended as a compact `[context update]` diff instead of re-embedding the whole file — only the changed instructions cost tokens. `/hot-reload <session-id>` resumes another session with a full rebuild (fresh system prompt included); session IDs autocomplete from disk.

Model, thinking level, preset prompt, and session-entered keys carry over in both modes.

### Providers

Built-in providers:

- `anthropic` — Anthropic Messages API
- `openai` — OpenAI Responses API
- `openai-codex` — OpenAI Codex login
- `amazon-bedrock` — AWS Bedrock ConverseStream
- `fireworks` — Fireworks OpenAI-compatible completions
- `openrouter` — OpenRouter completions gateway

`/provider` lists every provider, `/provider <name>` switches to it (model and matching key together), and `/provider <name> <api-key>` supplies a key when nothing is exported. Keys live in session memory and are saved to `~/.cupel/settings.json` (atomic write, owner-only permissions); they are never echoed. Resolution order: session key > environment variable > `~/.cupel/settings.json`. Switching models across providers via `/model` re-resolves the key the same way.

### Presets

A preset names a model, a thinking level, and optionally an additional system prompt. Define presets in the `model` section of `setting.json`:

```json
{
  "model": {
    "default": {
      "provider": "openai-codex",
      "model": "codex/gpt-6-sol",
      "thinkingLevel": "high"
    },
    "fast": {
      "provider": "anthropic",
      "model": "claude-haiku-5-5",
      "thinkingLevel": "medium",
      "prompt": "Answer in at most three sentences."
    }
  }
}
```

`provider`, `model` (an id from `/model`), and `thinkingLevel` (`off` ... `max`) are required. `prompt` is appended to the system prompt while the preset is active. `/preset fast` switches all three at once. The `default` preset also applies at startup.

### Themes

`/themes` lists the color themes and marks the active one. `/themes <id>` switches every session at once, including the messages already on screen. Typing `/themes ` offers the IDs in autocomplete. The choice is saved to `~/.cupel/settings.json`, so the next start uses it too:

```json
{
  "theme": "nightfox"
}
```

`cupel` is the only built-in theme and remains the default. The five palette themes are JSON examples in [`themes/`](themes/), not compiled into the binary. From the repository root, install them without overwriting existing files:

```bash
mkdir -p "${CUPEL_HOME:-$HOME/.cupel}/themes"
cp -n themes/*.json "${CUPEL_HOME:-$HOME/.cupel}/themes/"
```

Run `/hot-reload` to discover the files, then `/themes <id>` to switch. The palette themes below are available only after their JSON files are installed:

| ID | Name | Made for |
| -- | ---- | -------- |
| `cupel` | Cupel (default) | your terminal's own colors |
| `ayu-dark` | Ayu Dark | dark |
| `ayu-light` | Ayu Light | light |
| `dawnfox` | Dawnfox - opaque | light |
| `duskfox` | Duskfox - opaque | dark |
| `nightfox` | Nightfox - opaque | dark |

`cupel` draws in your terminal's own palette (its named colors plus three grays) on the terminal's own background. The other themes are *opaque*: they paint their own background on every cell, so they look the same in every terminal, whatever its color scheme or transparency. The palettes come from the Zed themes of the same names: the text and Markdown colors (headings, bold and italic, links) from Zed's editor, the rest from its terminal colors.

Only `~/.cupel/settings.json` sets the theme; a project's `.cupel/settings.json` cannot. An unknown ID starts cupel in the `cupel` theme with a warning. If saving fails, the switch still applies for this run and the notice explains why it was not saved. `/new` and `/hot-reload` keep the active theme; a hand-edited theme setting applies at the next start. Themes are TUI-only.

#### Defining a custom theme

A custom theme is a JSON file in `~/.cupel/themes/` (`$CUPEL_HOME/themes/` when `CUPEL_HOME` is set). The file name is the theme's ID: `~/.cupel/themes/gruvbox-dark.json` becomes `/themes gruvbox-dark`. Use lowercase letters, digits and dashes.

```json
{
  "name": "Gruvbox Dark",
  "appearance": "dark",
  "palette": {
    "bg": "#282828",
    "fg": "#ebdbb2",
    "surface": "#1d2021",
    "raised": "#3c3836",
    "selection": "#504945",
    "border": "#665c54",
    "muted": "#928374",
    "red": "#fb4934",
    "green": "#b8bb26",
    "yellow": "#fabd2f",
    "magenta": "#d3869b",
    "cyan": "#8ec07c",
    "heading": "#b8bb26",
    "emphasis": "#fe8019",
    "link": "#83a598"
  }
}
```

`name` is what `/themes` shows, and `appearance` is `"dark"` or `"light"`. The palette needs all 15 colors, each written `"#rrggbb"`:

| Palette color | Used for |
| ------------- | -------- |
| `bg` | the background of the whole screen |
| `fg` | ordinary text: your messages, the model's prose and reasoning |
| `surface` | code blocks and popups |
| `raised` | the background of your messages |
| `selection` | the background of the clicked block (Ctrl+O copies it) |
| `border` | pane borders and the idle input border |
| `muted` | the footer, titles, hints, diff context lines, the scrollbar |
| `red` | errors and removed diff lines |
| `green` | added diff lines |
| `yellow` | notices, the input border while the agent works, the scroll marker |
| `magenta` | the final answer |
| `cyan` | tool calls, inline code, list bullets, popup rows |
| `heading` | Markdown headings |
| `emphasis` | bold and italic text |
| `link` | link text |

cupel reads the folder at startup and again on `/new` and `/hot-reload`: after editing the file of the active theme, `/hot-reload` shows the change. `/themes` lists file themes after the built-in `cupel` theme, in file-name order, each with the file it came from. Edit `nightfox.json` to customize Nightfox; no rebuild is needed.

A file with a mistake (a missing or misspelled color, a value that isn't `#rrggbb`, a file name with capitals) is skipped, and a notice names the file and the problem. A theme whose text is hard to read on its backgrounds (contrast below 4.5:1) loads with a warning.

A project's `.cupel/themes/` is not read: the theme is a personal setting, like the `"theme"` entry, which only `~/.cupel/settings.json` sets.

To share another palette, add a JSON file to the repository's `themes/` folder. It must be copied into cupel home to be loaded; the repository folder is not read automatically.

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

### Guardrails

`cupel` intercepts the model's actions before they execute and enforces configurable safety rules. Two guardrails ship as examples of this mechanism:

- **Bash deny list** — shell commands are matched against regex rules before running; `rm -rf` (and its spellings: `-fr`, combined flags, behind `sudo` or `&&`) is blocked out of the box and the model receives an error naming the rule. Add your own rules — one regex per line, `#` comments — in `~/.cupel/bash-deny` (global) or `<project>/.cupel/bash-deny` (per project).
- **Loop killer** — blocks repeated identical tool calls after `maxRepeats` consecutive attempts, redirecting the model to a different approach. Configure it in `~/.cupel/settings.json` (global) or `<project>/.cupel/settings.json` (project overrides home):

```json
{
  "loopKiller": {
    "maxRepeats": 3
  }
}
```

## ACP clients

Build the separate protocol frontend with `cargo install --locked --path crates/cupel-acp`.
Configure the client's custom ACP command as `cupel-acp`, optionally followed by
`--model <id>` and `--thinking <level>`. The binary and Cupel's credentials must
be available in the environment where the client launches the agent. The client
supplies each session's absolute working directory; there is no `--cwd` flag.

The frontend keeps Cupel's tools, providers, project trust, hooks, and JSONL
transcripts. It supports text prompts, streamed tool results, cancellation,
multiple sessions, history replay, and context/cost updates. Only one session on
a connection may run in a given canonical directory at a time. Closing the
connection stops active work and settles lifecycle hooks.

Model selection is configured at process startup. Injected MCP servers, images,
structured user questions, and interactive login are unsupported; non-text
prompts and nonempty MCP server lists receive explicit errors. Ordinary assistant
questions work across chat turns. Trust a project through the ordinary Cupel TUI
before using its project hooks or sensitive model overrides; ACP never grants
trust automatically.

### Zed IDE

Following Zed's [External Agents documentation](https://zed.dev/docs/ai/external-agents#custom-agents),
run `agent: open settings` from the Command Palette, open **External Agents**, and
choose **Add Agent → Add Custom Agent**. Zed opens your settings file.

Merge the [settings example](crates/cupel-acp/examples/zed-settings.json) into your
existing settings without replacing other entries:

```json
{
  "agent_servers": {
    "Cupel": {
      "type": "custom",
      "command": "/absolute/path/to/cupel-acp",
      "args": [],
      "env": {}
    }
  }
}
```

Replace `command` with the absolute path printed by `command -v cupel-acp` after
installation. This avoids differences between your shell's and Zed's `PATH`.
Leave `args` empty to use Cupel's default model, or set it to
`["--model", "<id>", "--thinking", "medium"]`, replacing `<id>` with a Cupel catalog
model ID. If needed, set `CUPEL_HOME` in `env` to the absolute path of your Cupel
home directory. Configure provider credentials in Cupel's home or the agent's
environment; Zed's own LLM provider settings do not configure Cupel. Complete
interactive login and project trust in the Cupel TUI beforehand.

Open your project in Zed, then select **Cupel** from the Agent Panel's new-thread
menu. For SSH or container projects, ensure the binary, credentials, and project
path are available wherever Zed launches the agent.

Zed can forward configured MCP servers to external agents, but Cupel currently
rejects nonempty MCP server lists. If starting a thread reports
`MCP server injection is not supported by cupel-acp`, disable those MCP servers
in Zed for this setup. Run `dev: open acp logs` from the Command Palette to inspect
the protocol exchange when troubleshooting.

### OpenHands

OpenHands' Agent Server must have `cupel-acp` and Cupel's credentials available in
its execution environment; installing only on your laptop is not sufficient for
a remote or containerized Agent Server.

For a deterministic OpenHands integration check, run the
[smoke-test example](crates/cupel-acp/examples/openhands_smoke.py) from the repository
root with `uv` installed. It uses the real OpenHands SDK with local fake inference,
requires no model credentials, and verifies two turns with retained history:

```bash
cargo build --locked -p cupel-acp
uv run --python 3.13 --with openhands-sdk==1.58.1 \
  crates/cupel-acp/examples/openhands_smoke.py target/debug/cupel-acp
```

## Appendix

### Grep & Grep Rank benchmark

| Metric | Baseline (rg, published) | swe-grep (published) | pgr | cupel |
| ------ | ------------------------ | -------------------- | --- | ----- |
| MRR | 0,318 | 0,306 | 0,405 | 0,455 |
| Hit@1 | 26,0% | 18,0% | 34,0% | 40,0% |
| Hit@3 | 34,0% | 42,0% | 42,0% | 50,0% |
| Hit@5 | — | — | 52,0% | 54,0% |
| Output (tokens) | 6566 | 1427 | 1587 | 64 |
