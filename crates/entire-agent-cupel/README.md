# entire-agent-cupel

The [Entire CLI](https://github.com/entireio/cli) external-agent shim for
cupel, implementing [protocol version 1](https://github.com/entireio/cli/blob/main/docs/architecture/external-agent-protocol.md).
Entire manages AI coding sessions (checkpoints, transcripts, resume across
agents); this binary teaches it to work with cupel.

## How it fits together

cupel already does the heavy lifting (see the main README):

- **transcripts**: every session is a JSONL file under
  `~/.cupel/sessions/<project-slug>/<session-id>.jsonl`,
- **hooks**: executables in `.cupel/hooks/<event>/` run on
  `session-start`, `user-prompt-submit`, `stop`, `session-end` with a JSON
  payload on stdin,
- **resume**: `cupel --resume <session-id>`.

The shim translates between those and Entire's JSON-over-stdin/stdout
subcommands, reusing cupel's own `session` module so the two can never
disagree about the transcript format. `install-hooks` drops one forwarding
script per event into the project's `.cupel/hooks/` tree; each script is a
two-liner that pipes cupel's payload straight to `entire hooks cupel
<event>`.

Declared capabilities: `hooks` and `transcript_analyzer` (modified files,
prompts, positions). Position is the transcript's message count, not a byte
offset.

## Setup

```sh
cargo build --release -p entire-agent-cupel
cp target/release/entire-agent-cupel ~/.cupel/bin/   # any $PATH dir works
```

Then, in the repository where you use cupel with Entire:

```json
// .entire/settings.json
{ "external_agents": true }
```

Entire discovers the binary on PATH (`entire-agent-<name>`), validates it
via `info`, and installs the hook forwarders itself. Verify by hand with:

```sh
entire-agent-cupel detect
entire-agent-cupel get-session-dir --repo-path "$PWD"
```
