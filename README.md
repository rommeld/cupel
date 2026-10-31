<p align="center">
  <img width="300" height="330" alt="image" src="https://github.com/user-attachments/assets/a4174b06-a4b7-438e-bbcb-4a38ed98bd4f" />
</p>

# cupel

`cupel` is a lean Rust coding agent with a terminal UI, plain-text mode, and ACP
support. It combines provider-neutral inference with file reading, ranked grep,
patching, and shell tools.

## Install

### Prebuilt binary

No Rust required. macOS (Intel & Silicon) and Linux (x86_64/aarch64, static musl) via `curl`:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Installs `cupel` and `cupel-acp` into `~/.cupel/bin`; add it to your `PATH`.
Override the location with `CUPEL_HOME` or `CUPEL_INSTALL_DIR`.
Alternatively: `brew install rommeld/tap/cupel`.
Re-run the installer or use `brew upgrade cupel` to update both binaries;
the TUI's `/update` updates only `cupel`.

### From source

Requires a recent stable Rust toolchain:

```sh
git clone https://github.com/rommeld/cupel.git
cd cupel
cargo install --locked --path crates/cupel-tui
cargo install --locked --path crates/cupel-acp
```

## Usage

Run `cupel` in your project directory. Set a provider key (see below), or use
`/login openai-codex` for a ChatGPT subscription. Use `@path` to reference files,
`Esc` to cancel work, and `cupel --help` for CLI options.

```sh
cupel                                     # interactive TUI
cupel --resume                            # resume the latest session
printf 'Explain this project\n' | cupel --plain
```

### Slash commands

| Command | Description |
| ------- | ----------- |
| `/help` | List all commands and prompt templates |
| `/new` | Clear the conversation and start a fresh session |
| `/model <id>` | Switch model (no argument lists them) |
| `/provider <name> [api-key]` | Switch provider, optionally supplying a key (no argument lists them) |
| `/login openai-codex [device]` | Log in with a ChatGPT Plus/Pro subscription |
| `/thinking <level>` | Set thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `/preset <name>` | Switch model, thinking level, and prompt to a preset (no argument lists them) |
| `/themes [id]` | Switch the color theme (no argument lists them) |
| `/review [path...]` | Bundle the project, specific paths, or a `--diff` into a code-review prompt |
| `/agent <name> [task]` | Call a configured helper directly; without a task, send it your next prompt |
| `/team` | Manage helpers, inspect tasks and discussion, or cancel a subtree |
| `/usage` | Show token usage and estimated costs |
| `/hot-reload [session-id]` | Reload `.cupel` config into the running or another session |
| `/quit` | Quit cupel |

Most commands are TUI-only; plain mode supports `/help`, `/review`, `/quit`,
and prompt templates.

### Configuration and sessions

- Put instructions in `AGENTS.md` or `CLAUDE.md`, and prompt templates in
  `prompts/<name>.md` to expose `/<name>` with `$1`/`$@` argument substitution.
  Precedence: working directory > project `.cupel/` > `~/.cupel/`.
- Store settings in `~/.cupel/settings.json` or `<project>/.cupel/settings.json`.
  `CUPEL_HOME` overrides the home directory. `/hot-reload` applies config changes.
- Sessions are JSONL transcripts under `~/.cupel/sessions/`. Use `/session-id`
  to list them and `cupel --resume <session-id>` to resume one. Never open the
  same session in two processes at once.

### Providers

| Provider | Authentication |
| --- | --- |
| `anthropic` | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` |
| `openai` | `OPENAI_API_KEY` |
| `openai-codex` | `/login openai-codex` |
| `amazon-bedrock` | AWS credential chain or `AWS_BEARER_TOKEN_BEDROCK` |
| `fireworks` | `FIREWORKS_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

API keys entered through `/provider` are saved to home settings.
Resolution order: session key > environment variable > home settings.
For gateways such as LiteLLM, use `ANTHROPIC_BASE_URL` with
`ANTHROPIC_AUTH_TOKEN`, or `AWS_ENDPOINT_URL_BEDROCK_RUNTIME` with
`AWS_BEARER_TOKEN_BEDROCK` and `AWS_REGION`.

### Presets

A preset combines provider, model, thinking level, and an optional `prompt`.
Define it in `settings.json`; `default` also applies at startup:

```json
{
  "model": {
    "default": {
      "provider": "openai-codex",
      "model": "codex/gpt-6.1-sol",
      "thinkingLevel": "high"
    }
  }
}
```

Use IDs from `/model` and switch named presets with `/preset <name>`.

### Helper agents

Define helpers in home settings or a trusted project's `.cupel/settings.json`;
none are built in. For example:

```json
{
  "agents": {
    "scout": {
      "provider": "openai-codex",
      "model": "codex/gpt-6.1-sol",
      "thinkingLevel": "low",
      "description": "Finds relevant files and functions."
    }
  }
}
```

- `/agent scout <task>` calls a helper directly. `/team add scout` lets the lead
  call it; `/team remove scout` or `/team clear` removes that access.
- Teams start empty unless a helper sets `"team": true`. A `description` is
  required for team membership. Delegation remains the model's choice.
- Helpers default to `read` and `grep`. Explicit `tools` can also grant `bash`
  and `apply_patch`; both count as write access and are **not sandboxed**.
- Team members share task results and discussion, not private prompts or raw
  histories. Use `/team tasks`, `/team conversation`, and `/team cancel <id>`
  to inspect or stop work. Cancellation does not undo edits or incurred costs.
  In the TUI, Ctrl+B shows or hides live team updates. Headers identify each
  helper by name and the lead by its known model; the final answer stays below
  the updates. Task progress updates in place instead of repeating handoffs,
  helper tool starts show the caller and tool name, and results render Markdown.
- Optional `agentCommands` mappings route `/review` or prompt templates to a
  named helper. `/team roles` explains automatic role matching.

Helpers are TUI-only. Untrusted project helper definitions and mappings are ignored.

### Themes

`/themes <id>` switches themes and saves the choice in home settings. The default
`cupel` theme uses your terminal's colors. To install the [example palettes](themes/)
from the repository root:

```bash
mkdir -p "${CUPEL_HOME:-$HOME/.cupel}/themes"
cp -n themes/*.json "${CUPEL_HOME:-$HOME/.cupel}/themes/"
```

Run `/hot-reload`, then `/themes` to list them. Customize a palette by editing
`~/.cupel/themes/<id>.json` and reloading; project theme files are not loaded.

### Local models

With `ollama serve` running, pulled models appear in `/model` and `/provider`.
Discovery uses `OLLAMA_HOST` or `http://localhost:11434` and assumes a 4096-token
context window. To override it or add another OpenAI-compatible endpoint, put
an entry in `~/.cupel/models.json` or `<project>/.cupel/models.json`:

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

For `llama-server`, change `baseUrl` to `http://localhost:8080/v1`.
`requiresApiKey: false` marks a keyless endpoint.

### Guardrails

- Common `rm -rf` forms are blocked. Add regex rules (one per line) to
  `~/.cupel/bash-deny` or `<project>/.cupel/bash-deny`.
- Enable repeated-tool-call protection with `"loopKiller": {"maxRepeats": 3}`
  in home or project settings.
- Project trust gates hooks and sensitive configuration. These checks are
  **not an execution sandbox**; review changes and protect your credentials.

## ACP clients

`cupel-acp` exposes ACP v1 over stdio with text prompts, streamed tool activity,
cancellation, session restoration, and usage updates. Configure your client's
agent command as the absolute path to `cupel-acp`, optionally adding
`--model <id>` and `--thinking <level>`.

The binary and credentials must be available where the client runs it; the client
supplies the working directory. Complete login and project trust in the TUI first.
Images, injected MCP servers, structured user questions, and interactive login
are unsupported over ACP.

## More

- [Feature reference](FEATURES.md)
- [Contributing and workspace layout](CONTRIBUTING.md)
- [Releases and packaging](packaging/README.md)
- [Development benchmarks](scripts/benchmarks/)
- [Changelog](CHANGELOG.md) · [MIT license](LICENSE)
