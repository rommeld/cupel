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
- [Appendix](#appendix)

## Workspace crates

- **`cupel-core`** — provider-neutral chat-completion abstraction with a built-in model catalog, token/cost tracking, request/response tracing, and retry/backoff. The foundation for all LLM calls.
- **`cupel-agent`** — the agent loop: wires system prompt, message history, and tool definitions into repeated inference calls, executes tool calls, and feeds the results back. Includes context-compaction hooks and the `AgentHooks` extension point.
- **`cupel-coding-agent`** — the coding-agent library: slash commands, prompt templates from `prompts/<name>.md`, project context from `AGENTS.md`/`CLAUDE.md`, sessions, the line-based plain mode, and the built-in tools `read`, `grep` (backed by the `grep` crate family), `apply_patch`, and `bash`. It has no UI dependency, so other programs can embed it.
- **`cupel-tui`** — the `ratatui` TUI with `@file-path` fuzzy referencing, built on `cupel-coding-agent`. Ships the `cupel` CLI.

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
| `/usage` | Show session token and cost totals |
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

| ID | Name | Made for |
| -- | ---- | -------- |
| `cupel` | Cupel (default) | your terminal's own colors |
| `ayu-dark` | Ayu Dark | dark |
| `ayu-light` | Ayu Light | light |
| `dawnfox` | Dawnfox - opaque | light |
| `duskfox` | Duskfox - opaque | dark |
| `nightfox` | Nightfox - opaque | dark |

`cupel` draws in your terminal's own palette (its named colors plus three grays) on the terminal's own background. The other themes are *opaque*: they paint their own background on every cell, so they look the same in every terminal, whatever its color scheme or transparency. The palettes come from the Zed themes of the same names.

Only `~/.cupel/settings.json` sets the theme; a project's `.cupel/settings.json` cannot. An unknown ID starts cupel in the `cupel` theme with a warning. If saving fails, the switch still applies for this run and the notice explains why it was not saved. `/new` and `/hot-reload` keep the active theme; a hand-edited theme setting applies at the next start. Themes are TUI-only.

#### Defining a custom theme

Themes are compiled into cupel; there are no theme files. A custom theme therefore means building from source (see [From source](#from-source)). All themes live in `crates/cupel-tui/src/theme.rs`.

1. Define the theme from a palette of 13 colors. Every part of the screen takes one of them:

    ```rust
    pub const GRUVBOX_DARK: Theme = Theme::from_palette(
        "gruvbox-dark", // the /themes ID: lowercase letters and dashes
        "Gruvbox Dark", // the name /themes shows
        Appearance::Dark, // Dark or Light
        Palette {
            bg: hex(0x28_2828),
            fg: hex(0xeb_dbb2),
            surface: hex(0x1d_2021),
            raised: hex(0x3c_3836),
            selection: hex(0x50_4945),
            border: hex(0x66_5c54),
            muted: hex(0x92_8374),
            red: hex(0xfb_4934),
            green: hex(0xb8_bb26),
            yellow: hex(0xfa_bd2f),
            blue: hex(0x83_a598),
            magenta: hex(0xd3_869b),
            cyan: hex(0x8e_c07c),
        },
    );
    ```

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
    | `blue` | links |
    | `magenta` | the final answer |
    | `cyan` | tool calls, inline code, list bullets, popup rows |

2. Add the constant to `THEMES` in the same file. `/themes` lists the themes in that order, and autocomplete offers the ID.

3. Run the theme tests. They check that the ID is unique and well formed, and that `fg` is readable (contrast of at least 4.5:1) on `bg`, `raised`, and `surface`:

    ```sh
    cargo test -p cupel-tui theme
    ```

4. Install the build with `cargo install --path crates/cupel-tui` and switch with `/themes gruvbox-dark`.

To change one part of an existing theme, start from it and override that field; `..NIGHTFOX` copies every field you do not list. Each field of `Theme` has a doc comment saying what it styles:

```rust
pub const BOLD_NIGHTFOX: Theme = Theme {
    id: "bold-nightfox",
    name: "Nightfox with a bold answer",
    answer: NIGHTFOX.answer.add_modifier(Modifier::BOLD),
    ..NIGHTFOX
};
```

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

## Appendix

### Grep & Grep Rank benchmark

| Metric | Baseline (rg, published) | swe-grep (published) | pgr | cupel |
| ------ | ------------------------ | -------------------- | --- | ----- |
| MRR | 0,318 | 0,306 | 0,405 | 0,455 |
| Hit@1 | 26,0% | 18,0% | 34,0% | 40,0% |
| Hit@3 | 34,0% | 42,0% | 42,0% | 50,0% |
| Hit@5 | — | — | 52,0% | 54,0% |
| Output (tokens) | 6566 | 1427 | 1587 | 64 |
