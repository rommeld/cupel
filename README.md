<p align="center">
  <img width="300" height="300" alt="Cupel mascot sitting cross-legged with a Bembel and an apple wine glass" src=".github/cupel-logo.png" />
</p>

# cupel

`cupel` is a coding agent written in Rust. It works with multiple model providers. It reads and
searches files, applies patches, and runs shell commands. It can rank files that match a search.
It runs independent tool calls in parallel.

## Getting started

Install `cupel` and `cupel-acp` on macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
```

The installer puts both commands in `~/.cupel/bin` by default. Add that directory to your `PATH`.
Set `CUPEL_HOME` to change Cupel's home directory or `CUPEL_INSTALL_DIR` to change only where the
installer puts the commands.

If you used the installer, run `/update` in the terminal interface to update and restart `cupel`.
Run the installer again to update both binaries. To skip the startup update check, set
`"checkForUpdates": false` in your home settings.

### From source

To build and install both commands, use a recent stable Rust toolchain:

```sh
git clone https://github.com/rommeld/cupel.git
cd cupel
cargo install --locked --path crates/cupel-tui
cargo install --locked --path crates/cupel-acp
```

## Usage

Run `cupel` in your project directory. To use a hosted model, configure your [provider](#providers)
or sign in with `/login openai-codex` to use a ChatGPT subscription. You can also use
[local models](#local-models).

```sh
cupel                                     # start an interactive session
cupel --resume                            # resume the latest session
cupel --resume "fix the login bug"         # resume a session by part of its opening prompt
printf 'Explain this project\n' | cupel --plain
```

Set `--model <id>` or `--thinking <level>` to override the startup settings. Run `cupel --help` to
list options and model IDs. Cupel uses plain mode when you pass `--plain` or redirect input or output.
It reads piped input as one prompt.

### Terminal controls

- Use `@path` to reference a file. Press `Tab` to accept a completion and use `Up` and `Down` to
  browse prompt history when no menu is open. When the agent asks a question, choose an answer in the
  terminal interface.
- Press `Enter` to send a prompt or queue it while the agent works. Press `Ctrl+Enter` or `Ctrl+J` to
  send a follow-up to the running agent without waiting for the turn to end. Use `Shift+Enter` to
  insert a newline. Modified Enter keys require terminal support.
- Press `Esc` to cancel work. When idle, press it twice within 500 milliseconds to clear the unsent
  prompt. Use `Alt+Up` to bring queued prompts back into the input.
- Press `Ctrl+T` to change how much tool output is shown, `Ctrl+R` to expand or collapse reasoning,
  and `Ctrl+O` to copy the selected text or latest answer. Use `Ctrl+Y` to toggle mouse capture.
- Run `/help` to list commands, prompt templates, and prompt shortcuts. Use `/quit` or `/exit` to close
  Cupel.

### Tools and edits

The agent can read images for models that accept them. Its search tool can list matching files with
definitions first and tests last. A patch can check that a file still matches the version the agent
read before it writes changes.

Use `/changes` to see the most recent turn with patch edits. Use `/undo` to reverse those edits.
Both commands work in the terminal interface and plain mode. Neither tracks edits or other side
effects from shell commands. Undo refuses to overwrite newer file changes.

On Unix, the agent can ask Cupel to return a job ID when it starts a Bash command or if the command
runs past a time limit set for the wait. The agent uses the `jobs` tool to check output, wait for the
result, or stop the job. Canceling a wait leaves the command running. Cupel stops managed jobs when
the session reloads or ends. If Bash finishes before returning a job ID, normal shell exit does not
stop its descendants.

On timeout or cancellation, Bash cleanup is best effort. On macOS and Linux, Cupel also looks for
descendants in other process groups within the command's Unix session. This requires `/bin/ps`, plus
process file descriptors on Linux or Apple's private process identity API on macOS. Incomplete
cleanup is reported in foreground output or in a managed job's `cleanupError`. On other Unix
platforms, cleanup only targets the original process group. Processes that create a new Unix session
are outside this cleanup boundary.

When Bash reports a full-output log path, the file remains readable until the session closes
normally, even if the command failed. A crash can leave the file behind.

### Configuration and sessions

Put instructions in `AGENTS.md` or `CLAUDE.md`. Cupel looks for instructions and prompt templates in
your working directory, its `.cupel/` directory, and `~/.cupel/`. Save a prompt template as
`prompts/<name>.md` to run it as `/<name>`. Use `$1` for the first argument or `$@` for all arguments.
If two templates have the same name, Cupel uses the first one in the directory order above.

Store settings in `~/.cupel/settings.json` or `<project>/.cupel/settings.json`. Set `CUPEL_HOME` to
use a different Cupel home directory. Project presets and tool repeat limits override home settings.
Put provider keys, theme choices, and update preferences in the home settings file. Cupel ignores
those fields in the project settings file.

Cupel stores session transcripts as JSON Lines files under `~/.cupel/sessions/`. Use `/sessions` to
list saved sessions for the current project. Use `/sessions <search>` to match text anywhere in the
opening prompt, regardless of case. Resume a match with `/hot-reload <search>` or
`cupel --resume "search"`. The search must match exactly one session. Do not open the same session
in two processes at once.

Use `/hot-reload` to reload configuration and resume the latest saved session. If that session is
already open or no saved session exists, Cupel reloads in place. Use `/new` to start with no
conversation history or `/clear` to clear displayed output without losing the conversation.
Cupel trims older tool output as the conversation approaches the model's context limit and
summarizes older messages if it needs more room. Use `/usage` to see token counts and estimated
cost by model for the current session and across saved sessions.

### Skills

Put a `SKILL.md` file in `.cupel/skills/<name>/` or `~/.cupel/skills/<name>/`. Give it a `name` and
`description` in its YAML header. Cupel shows the agent the available skill descriptions and reads
the instructions only when the agent uses the skill.

A project skill overrides a home skill with the same name. Cupel ignores the old `disable` field;
to hide a skill, move its `SKILL.md` out of the skill directories. Start a new session or run
`/hot-reload` to refresh the list. Cupel caches local Markdown guides linked from instructions or
skills when it loads the file that links to them.

### Git worktree sessions

Run `/spinoff <name> [preset]` from the main Git checkout to start a parallel session in a separate
worktree. The worktree starts at the current commit. It does not include uncommitted changes from the
main checkout. Run `/spinoff` to list worktrees. Use `Ctrl+N`, `Ctrl+P`, or the sidebar to switch
sessions. Cupel restores these sessions when you restart it from the main checkout.

Use `/spinoff merge <name> [message]` to bring the changes back into the main checkout. Use
`/spinoff drop <name>` to remove the worktree and branch without merging.

### Providers

| Provider | Authentication |
| --- | --- |
| `anthropic` | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` |
| `openai` | `OPENAI_API_KEY` |
| `openai-codex` | `/login openai-codex` |
| `amazon-bedrock` | AWS credential chain or `AWS_BEARER_TOKEN_BEDROCK` |
| `fireworks` | `FIREWORKS_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `google` | `GOOGLE_API_KEY` or `GEMINI_API_KEY` |

Run `/provider` to list providers or `/provider <name>` to switch. Cupel saves keys you enter with
`/provider <name> <api-key>` in your home settings. It checks for a key in this order: the current
session, the environment, then home settings. For Gemini, `GOOGLE_API_KEY` takes precedence over
`GEMINI_API_KEY`.

To sign in without a browser callback, use `/login openai-codex device`. Run
`/logout openai-codex` to remove a saved login.

For a gateway such as LiteLLM, use `ANTHROPIC_BASE_URL` with `ANTHROPIC_AUTH_TOKEN`, or set
`AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, `AWS_BEARER_TOKEN_BEDROCK`, and `AWS_REGION`.

### Presets

A preset sets the provider, model, and thinking level. Define presets in `settings.json`:

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

Cupel applies the `default` preset at startup. Run `/model` to list model IDs or `/model <id>` to
switch models. Use `/preset <name>` to switch to a named preset. Use `/thinking <level>` to change
the thinking level. Cupel accepts `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`.

Cupel keeps legacy `prompt` text in settings but does not send it to the model. Put instructions
that apply across presets in `AGENTS.md` or `CLAUDE.md`.

### Themes

Use `/themes <id>` to switch themes. Cupel saves your choice in home settings. The default `cupel`
theme uses your terminal's colors.

To install the [example palettes](themes/), run these commands from the repository root:

```bash
mkdir -p "${CUPEL_HOME:-$HOME/.cupel}/themes"
cp -n themes/*.json "${CUPEL_HOME:-$HOME/.cupel}/themes/"
```

Run `/hot-reload`, then use `/themes` to list the available themes.

#### Defining a custom theme

Copy an [example palette](themes/arcade-light.json) to `~/.cupel/themes/<id>.json`. Edit its name,
appearance, and colors. Use `light` or `dark` for `appearance` and `#rrggbb` values for colors.
Set `palette.reasoning_header` to change the color of reasoning headings without changing the body
text. The filename without `.json` is the theme ID. Run `/hot-reload` to load the file. Cupel does
not load theme files from the project directory.

### Local models

Start `ollama serve` to see the models you've downloaded in `/model` and `/provider`. Cupel looks for
Ollama at `OLLAMA_HOST` or, if it's unset, at `http://localhost:11434`. Cupel assumes a context window
of 4,096 tokens for models it finds this way.

To change the context window or add another OpenAI-compatible endpoint, add an entry to
`~/.cupel/models.json` or `<project>/.cupel/models.json`:

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

For `llama-server`, change `baseUrl` to `http://localhost:8080/v1`. Set `"requiresApiKey": false` if
the endpoint does not need an API key.

### Guardrails

- Cupel blocks common forms of `rm -rf`. To block more commands, add one regular expression per line
  to `~/.cupel/bash-deny` or `<project>/.cupel/bash-deny`.
- To limit repeated tool calls, add `"loopKiller": {"maxRepeats": 3}` to home or project settings.
- Cupel runs project hooks and loads sensitive project model settings only after you mark the project
  as trusted. These checks do not sandbox tool execution. Review changes and protect your credentials.

### Hooks

To run a script on a session event, put an executable file in `~/.cupel/hooks/<event>/` or in a
trusted project's `.cupel/hooks/<event>/`. Cupel supports `session-start`, `user-prompt-submit`,
`stop`, and `session-end`. It passes a JSON object to the script on standard input and clears the
inherited environment. Hooks can observe events but cannot block them.

## ACP clients

`cupel-acp` implements version 1 of the Agent Client Protocol over standard input and output. Clients
can send text prompts, cancel work, and restore sessions. They receive tool activity and usage updates
as the agent runs.

Set your client's agent command to the absolute path to `cupel-acp`. You can add `--model <id>` and
`--thinking <level>`. The client supplies the working directory. Make the binary and credentials
available where the client runs it. If you use a subscription, sign in through `cupel` first. Use
`cupel` to confirm trust before using project hooks or sensitive model settings.

See the [client setup examples](crates/cupel-acp/examples/) for Zed and OpenHands.

## More

- [Contributing and workspace layout](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)
