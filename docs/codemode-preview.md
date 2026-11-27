# Test codemode

Build and check the development binaries on Apple Silicon macOS. The feature
is off by default and is not enabled in release builds. Python 3.11 or later
is needed for the offline checks, not to run a script in Cupel.

```sh
cargo build --locked -p cupel-tui -p cupel-acp --features cupel-tui/codemode-preview,cupel-acp/codemode-preview
python3 crates/cupel-tui/tests/codemode_preview.py target/debug/cupel --acp target/debug/cupel-acp
cargo test --locked -p cupel-tui --features codemode-preview --lib codemode_reload
```

The runner uses a loopback provider and a disposable checkout. It checks the
frozen direct declarations, observation handoff, cancellation, and result
privacy without inference credentials. The TUI test checks settings reload
and observation lifetimes. Do not try a live model if any check fails.

## Enable for a local trial

For a one-time trial, run `cupel --plain --codemode-preview` on Apple Silicon
macOS. Both flags are required when using the explicit preview flag.

To opt in for plain mode, TUI, and ACP in feature-enabled builds, add this
section to `~/.cupel/settings.json` or `$CUPEL_HOME/settings.json`:

```json
{"codemode": {"enabled": true}}
```

Absent or `false` leaves the original tools and prompt unchanged. A project
settings file cannot enable codemode or override the home choice. An invalid
home value produces a warning and leaves codemode off. TUI `/hot-reload`
refreshes the tool list and instruction snapshot. An observation survives
an in-place reload with the same recorder, but not resuming a transcript.

## Try a search and read

Use a disposable checkout and a configured model. The guest has no direct
filesystem, network, or subprocess access. The two file callbacks have the
same permissions as direct `read` and `grep`; they are not checkout-confined.

Ask the model to call `codemode` with `{"code":"…"}` and a script such as the
following. Monty supports a Python subset that includes loops, comprehensions,
lists, dictionaries, and string operations. Use `emit` with JSON-compatible
values. Cupel does not return `print` output.

```python
files = grep(pattern="Widget", outputMode="files", limit=3)["files"]
page = read(path=files[0]["path"], limit=3)
lines = [line for line in page["text"].splitlines() if "Widget" in line]
emit({"path": page["path"], "lines": lines, "observation": page["observation"]})
```

Record whether the model chose codemode, whether the selected path and excerpt
are correct, and whether the model retried with direct tools. If the model
only used direct tools, record the trial as a run without codemode. See
[worker budgets](codemode-worker.md) for limits. A local trial is not release
qualification.
