# Launching the packaged ACP frontend

Release archives containing ACP packaging include `cupel`, `cupel-acp`, and this
directory. No Rust toolchain is required. Install both binaries with:

```sh
curl -fsSL https://raw.githubusercontent.com/rommeld/cupel/main/install.sh | sh
"${CUPEL_INSTALL_DIR:-${CUPEL_HOME:-$HOME/.cupel}/bin}/cupel-acp" --help
```

Alternatively, extract the archive for your platform, verify it against the
release's `sha256sums.txt`, and copy both executables to a directory on `PATH`.
Homebrew users can use `brew install rommeld/tap/cupel` after the bundled release.
Older releases do not include `cupel-acp`; use a release that contains it.

## Client configuration and credentials

Use the absolute path printed by `command -v cupel-acp` as the agent command.
The installer's default is `$HOME/.cupel/bin/cupel-acp`. Shell substitutions and
`~` are not expanded in JSON: replace them with the actual absolute path.

- **Zed:** merge `zed-settings.json` into your settings and replace its command
  placeholder with the installed path. Optional arguments are
  `["--model", "<catalog-id>", "--thinking", "medium"]`.
- **OpenHands SDK:** use
  `ACPAgent(acp_command=["/absolute/path/to/cupel-acp"])`; the Agent Server must
  have that executable and its credentials in its execution environment.
- **SSH/container workspaces:** install inside the remote host/container, not
  just on your laptop, and use the executable path in that environment. The
  client supplies an absolute session working directory there.

Use Cupel's existing provider environment variables (for example
`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`) or its home configuration in
`~/.cupel` (`CUPEL_HOME` overrides it). To reuse a login, launch the regular
`cupel` TUI in that same environment and complete `/login openai-codex` first.
Complete project trust there as well if project hooks or sensitive model
overrides are needed. ACP does not add a login mechanism; editor LLM settings
are not Cupel credentials. Do not embed secrets in client examples.

## Offline checks

From an extracted release archive, with Python 3 installed:

```sh
python3 examples/stdio_smoke.py ./cupel-acp
```

This checks initialize, session creation, a streamed response from local fake
inference, and clean shutdown. It uses a temporary Cupel home without real keys.

For a real OpenHands integration check with two turns and retained history:

```sh
uv run --python 3.13 --with openhands-sdk==1.58.1 \
  examples/openhands_smoke.py ./cupel-acp
```

Re-run the installer (or `brew upgrade cupel`) to update both executables.
The TUI's `/update` continues to replace only `cupel`.
