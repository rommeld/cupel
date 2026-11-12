<p align="center">
  <img width="300" height="330" alt="image" src="https://github.com/user-attachments/assets/a4174b06-a4b7-438e-bbcb-4a38ed98bd4f" />
</p>

# cupel

A cupel is a small vessel for refining precious metals. This project borrows that idea: it separates useful code context from repository noise, then feeds that refined signal into fast, local agent workflows.

`cupel` is a lean coding agent written in Rust. It combines provider-neutral inference with file reading, ranked grep,
patching, and shell tools. A single agent runs independent tool calls in parallel.

## Getting started

Install the command-line interface:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

Installs `cupel` and `cupel-acp` into `~/.cupel/bin`; add it to your `PATH`. Override the location with `CUPEL_HOME` or `CUPEL_INSTALL_DIR`.

### From source

Requires a recent stable Rust toolchain:

```sh
git clone https://github.com/rommeld/cupel.git
cd cupel
cargo install --locked --path crates/cupel-tui
cargo install --locked --path crates/cupel-acp
```

## Usage

Run `cupel` in your project directory. Set a provider key (see below), or use `/login openai-codex` for a ChatGPT subscription. Use `@path` to reference files, `Esc` to cancel work, and `cupel --help` for CLI options.

```sh
cupel                                     # interactive TUI
cupel --resume                            # resume the latest session
cupel --resume "fix the login bug"         # find a session by its opening prompt
printf 'Explain this project\n' | cupel --plain
```

### Configuration and sessions

- Put instructions in `AGENTS.md` or `CLAUDE.md`, and prompt templates in `prompts/<name>.md` to expose `/<name>` with `$1`/`$@` argument substitution.
  Precedence: working directory > project `.cupel/` > `~/.cupel/`.
- Store settings in `~/.cupel/settings.json` or `<project>/.cupel/settings.json`. `CUPEL_HOME` overrides the home directory. `/hot-reload` applies config changes.
- Sessions are JSONL transcripts under `~/.cupel/sessions/`. Use `/sessions [search]` to find them by their opening prompt, then `/hot-reload <prompt search>` or
  `cupel --resume "prompt search"` to resume one. Searches are case-insensitive, use the full opening prompt, and must match exactly one session. Never open
  the same session in two processes at once.

### Providers

| Provider | Authentication |
| --- | --- |
| `anthropic` | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` |
| `openai` | `OPENAI_API_KEY` |
| `openai-codex` | `/login openai-codex` |
| `amazon-bedrock` | AWS credential chain or `AWS_BEARER_TOKEN_BEDROCK` |
| `fireworks` | `FIREWORKS_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |

API keys entered through `/provider` are saved to home settings. Resolution order: session key > environment variable > home settings.
For gateways such as LiteLLM, use `ANTHROPIC_BASE_URL` with `ANTHROPIC_AUTH_TOKEN`, or `AWS_ENDPOINT_URL_BEDROCK_RUNTIME` with `AWS_BEARER_TOKEN_BEDROCK` and `AWS_REGION`.

### Presets

A preset combines provider, model, thinking level, and an optional `prompt`. Define it in `settings.json`:

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

### Themes

`/themes <id>` switches themes and saves the choice in home settings. The default `cupel` theme uses your terminal's colors. To install the [example palettes](themes/) from the repository root:

```bash
mkdir -p "${CUPEL_HOME:-$HOME/.cupel}/themes"
cp -n themes/*.json "${CUPEL_HOME:-$HOME/.cupel}/themes/"
```

Run `/hot-reload`, then `/themes` to list them. Customize a palette by editing `~/.cupel/themes/<id>.json` and reloading; project theme files are not loaded.

### Local models

With `ollama serve` running, pulled models appear in `/model` and `/provider`. Discovery uses `OLLAMA_HOST` or `http://localhost:11434` and assumes a 4096-token
context window. To override it or add another OpenAI-compatible endpoint, put an entry in `~/.cupel/models.json` or `<project>/.cupel/models.json`:

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

For `llama-server`, change `baseUrl` to `http://localhost:8080/v1`. `requiresApiKey: false` marks a keyless endpoint.

### Guardrails

- Common `rm -rf` forms are blocked. Add regex rules (one per line) to `~/.cupel/bash-deny` or `<project>/.cupel/bash-deny`.
- Enable repeated-tool-call protection with `"loopKiller": {"maxRepeats": 3}` in home or project settings.
- Project trust gates hooks and sensitive configuration. These checks are **not an execution sandbox**; review changes and protect your credentials.

## ACP clients

`cupel-acp` exposes ACP v1 over stdio with text prompts, streamed tool activity, cancellation, session restoration, and usage updates. Configure your client's
agent command as the absolute path to `cupel-acp`, optionally adding `--model <id>` and `--thinking <level>`.

The binary and credentials must be available where the client runs it; the client supplies the working directory. Complete login and project trust in the TUI first.

## More

- [Contributing and workspace layout](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)
