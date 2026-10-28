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

Starting with the next release containing ACP packaging, each platform archive
includes **both `cupel` and `cupel-acp`**, covered by `sha256sums.txt`. The installer
puts both in `~/.cupel/bin` (`$CUPEL_HOME/bin`, or `$CUPEL_INSTALL_DIR` when set).
Run `~/.cupel/bin/cupel-acp --help` to verify the ACP executable. Older releases
still install `cupel` and warn when they do not yet include `cupel-acp`.
Homebrew installs both with `brew install rommeld/tap/cupel` after that release.
Re-run the installer, or use `brew upgrade cupel`, to update both binaries;
Cupel's existing `/update` command updates only `cupel`.

### From source

Requires a recent stable Rust toolchain:

```sh
git clone https://github.com/rommeld/cupel.git
cd cupel
cargo install --path crates/cupel-tui
cargo install --path crates/cupel-acp
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
| `/preset <name>` | Switch model, thinking level, and prompt to a preset (no argument lists them) |
| `/themes [id]` | Switch the color theme (no argument lists them) |
| `/review [path...]` | Bundle the project, specific paths, or a `--diff` into a code-review prompt |
| `/agent <name> [task]` | Call a configured helper directly; without a task, send it your next prompt |
| `/team [add\|remove <name>...\|clear]` | List configured helpers or change who the lead may call |
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

#### LLM gateways

An LLM gateway such as LiteLLM usually serves two formats. Configure either or both, then switch between them with `/provider anthropic` and `/provider amazon-bedrock`:

| Format | Variables | Effect |
| --- | --- | --- |
| Anthropic Messages | `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` | Every `anthropic` model posts to `<ANTHROPIC_BASE_URL>/v1/messages` and sends the token as `Authorization: Bearer`. The token wins over `ANTHROPIC_API_KEY`, which would go out as `x-api-key`. |
| Bedrock ConverseStream | `AWS_BEARER_TOKEN_BEDROCK`, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME` (for LiteLLM `https://<gateway>/bedrock`), `AWS_REGION` | Every `amazon-bedrock` model without its own `baseUrl` posts to `<endpoint>/model/<id>/converse-stream` and sends the token as `Authorization: Bearer`. No AWS keys or profile are needed. |

The Anthropic variables are the ones Claude Code reads, so a machine set up for Claude Code works as it is. For the Bedrock format, cupel reads the AWS SDK's standard variables rather than Claude Code's `CLAUDE_CODE_USE_BEDROCK` and `ANTHROPIC_BEDROCK_BASE_URL`.

Model ids go to the gateway unchanged. If it names its models differently, add `models.json` rows with `"provider": "anthropic"` (or `"amazon-bedrock"`) and the gateway's name as `id`, and copy `reasoning`, `thinkingLevelMap`, and `compat` from the matching row in `crates/cupel-core/src/catalog.json`.

### Presets

A preset names a model, a thinking level, and optionally an additional system prompt. Define presets in the `model` section of `settings.json`:

```json
{
  "model": {
    "default": {
      "provider": "openai-codex",
      "model": "codex/gpt-6.1-sol",
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

### Helper agents

Helpers are entirely user-defined: cupel ships no agent definitions or reserved
names. Define them in `~/.cupel/settings.json` or a trusted project's
`.cupel/settings.json`. These names are examples, not built-ins:

```json
{
  "agents": {
    "scout": {
      "provider": "openai-codex",
      "model": "codex/gpt-6.1-sol",
      "thinkingLevel": "low",
      "description": "Finds the files and functions relevant to a question.",
      "remember": false
    },
    "critic": {
      "provider": "openai-codex",
      "model": "codex/gpt-6.1-sol",
      "thinkingLevel": "high",
      "description": "Reviews diffs for correctness bugs.",
      "tools": ["read", "grep", "bash"],
      "maxTurns": 20,
      "prompt": "Report actionable findings with file paths."
    }
  },
  "agentCommands": {"review": "critic"}
}
```

Model fields follow presets. Names use `a-z`, `0-9`, `-`, and `_`. An omitted
`tools` grants only `read` and `grep`; available tools are `read`, `grep`, `bash`,
and `apply_patch`. Both `bash` and `apply_patch` count as write access, not a
filesystem sandbox. `maxTurns` must be positive. Helpers remember their private
history unless `remember` is false. A missing description makes a helper
manual-only: `/agent` and explicit command mappings can reach it, but it cannot
join a team. Project definitions replace same-name home definitions whole;
untrusted project definitions **and command mappings** are ignored with warnings.

`/agent scout find the parser` runs that helper without a lead turn. `/agent scout`
arms it for one prompt; Esc disarms it. While a direct helper runs, lead prompts
and one further helper call can wait. Esc cancels work, not edits already made.
Successful and failed exchanges inform the lead of answers and observed changes;
change reports also include concurrent user edits and explicitly report uncertainty.

Fresh teams start empty unless eligible definitions explicitly set `"team": true`.
Missing or false does not prevent explicit additions. `/team add scout critic` enables `ask` for read-only
members and `delegate` for writers; `/team remove <name>...` and `/team clear`
remove them. Eligibility alone does not add a member. Changing the tool schemas
affects prompt caching from the next lead run. During lead delegation, the first
Esc stops outstanding helpers; a second stops the lead. Restored sessions retain
their saved choices, including an empty team.

Start the normal TUI with one of `cupel --team [--add <name>...]`,
`cupel --agent <name> [task]`, or `cupel --review`. These mutually exclusive
forms use the same handlers as slash commands and do not exit automatically.
Use `--` before flag-shaped task words. They require terminal input and output;
`--plain` is not supported. `/new` and fresh spinoffs use their own settings'
defaults; resumed sessions and in-place reloads retain saved choices instead.
Default membership alone does not write a transcript until the first interaction.

`agentCommands` routes `/review` and prompt templates explicitly, independently of
team membership. Invalid mappings warn and fall back to the lead, not an armed
helper. Mappings cannot reroute non-prompt built-ins such as `/model`.

No helper Agent or model request is created until its first call. Startup can
still inspect credentials and reject unknown models. Each helper has its own
guard, loop killer and provider history. Helper transcripts are loaded lazily
under `<session-id>/<name>.jsonl` beside the lead transcript and archived with it.
The lead gets bounded task excerpts and answers; long answers are saved to files
when persistence is available. `/team` shows helper spend; the footer and the
`/usage` helper breakdown include incurred helper usage, even from failed calls.
The existing model-table popup remains the lead's current-session table.

The team shares an ordered, persistent discussion, not private system prompts,
thinking, or raw tool histories. `team_read` pages through it; `team_post` posts
or replies without waking another model. `/team conversation [after <id>]`
inspects it in the UI. Runtime-assigned authors and task references identify
each contribution. Posts are limited to 30 lines/1 KB; request-time snapshots
are bounded to 60 lines/4 KB and never accumulate in private histories.
On-team outcomes use this thread instead of duplicate legacy exchange notes;
off-team direct calls remain private to the caller and lead. Removed members
immediately lose thread tools. Clearing membership does not erase the discussion:
the lead retains bounded awareness of earlier outcomes. The log lives under
`<session-id>/team/conversation*.jsonl`, archives with the session, and falls back
to memory with a visible warning if persistence fails.

Active teammates can themselves `ask` or `delegate` to other current members.
Every descendant shares its root's limits: three helper levels, 16 helper calls,
64 provider requests (including retries and summarization), and a ten-minute
deadline. Read-only subtrees cannot acquire write access. Calls to busy helpers
queue in order; ancestor calls and dependency cycles are rejected before waiting.
Actual `bash`/`apply_patch` execution shares a checkout-wide write gate, released
before any caller waits for a child. This coordination is not a filesystem sandbox.
`/team tasks [root-id]` shows task states, parents, changes, and own versus subtree
costs; `/team cancel <id>` stops a subtree without rolling back edits or spend.
Interrupted tasks restore as interrupted, not runnable work. Usage checkpoints
retain known interrupted-call spend and flag incomplete observations.

Helper commands are interactive TUI features; plain mode is unchanged. Models
are not guaranteed to delegate just because a matching helper is available.

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

Install the packaged `cupel-acp` through the [prebuilt installer](#prebuilt-binary)
or Homebrew; no separate Rust installation is needed. For source builds, use
`cargo install --locked --path crates/cupel-acp`.
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
uv run --python 3.13 --with openhands-sdk==1.58.1 \
  crates/cupel-acp/examples/openhands_smoke.py "$(command -v cupel-acp)"
```

For a stdlib-only ACP check (no SDK or `uv`), run
`python3 crates/cupel-acp/examples/stdio_smoke.py "$(command -v cupel-acp)"`.
Both scripts are included under `examples/` in the release archive; see the
[client examples](crates/cupel-acp/examples/README.md) for launch and credential setup.
In a remote workspace or container, run the installer **inside that environment**
and configure the client with its absolute `cupel-acp` path there. Provide that
environment with the existing provider variables or Cupel home configuration;
do not assume it inherits your laptop's credentials or binaries.

## Appendix

### Grep & Grep Rank benchmark

| Metric | Baseline (rg, published) | swe-grep (published) | pgr | cupel |
| ------ | ------------------------ | -------------------- | --- | ----- |
| MRR | 0,318 | 0,306 | 0,405 | 0,455 |
| Hit@1 | 26,0% | 18,0% | 34,0% | 40,0% |
| Hit@3 | 34,0% | 42,0% | 42,0% | 50,0% |
| Hit@5 | — | — | 52,0% | 54,0% |
| Output (tokens) | 6566 | 1427 | 1587 | 64 |
