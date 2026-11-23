# Contributing to cupel

Contributions are welcome: bug reports, documentation, tests, provider support,
and improvements to the CLI, TUI, agent, or tools.

Not sure where to start? Open an issue or a draft pull request. Small,
well-explained improvements are a great starting point.

## Getting started

1. Fork the repository and create a branch for your change.
2. Install Rust with rustup; the repository pins its toolchain, rustfmt, and Clippy.
3. Read the [README](README.md) and [coding guidelines](AGENTS.md).

Build and run locally:

```sh
cargo check --workspace
cargo run -p cupel-tui
```

Running an actual agent requires a configured provider or local model.

## Where things belong

- `cupel-core`: provider integrations, streaming, model types, and usage tracking.
- `cupel-agent`: the agent loop, tool execution, cancellation, and compaction.
- `cupel-coding-agent`: coding tools, configuration, sessions, and plain mode.
- `cupel-tui`: the terminal interface and CLI entry point.

Keep shared behavior outside the TUI so other frontends can reuse it.

## Making a change

- Keep changes focused. Discuss larger features or behavior changes in an issue first.
- Follow existing patterns and avoid unrelated cleanup.
- Add regression tests for bug fixes and tests for new behavior.
- Prefer mock providers and local fixtures over paid API calls in automated tests.
- Preserve provider-neutral interfaces, predictable tooling, and existing safety boundaries.
- Update documentation when commands, settings, or user-visible behavior change.

Before submitting, run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

### Advisory Rust comment check

Pull requests get a warning for each new or rewritten ordinary Rust comment.
Rust doc comments are excluded, and warnings do not fail CI. Keep comments
that document a verified constraint or satisfy a legal or toolchain requirement.
Use the [no-comments skill](.cupel/skills/no-comments/SKILL.md) as the review guide.

To run the same comparison locally, fetch your target branch and pass it along
with the committed head you want to check:

```sh
git fetch origin main
cargo run --locked -p no-new-comments -- --base origin/main --head HEAD
```

The tool compares the merge base with the committed head. It ignores staged
and unstaged edits and reports line numbers from the head commit.

## Issues and pull requests

For bugs, include your cupel version, operating system, reproduction steps,
and expected versus actual behavior. For inference problems, include the
provider and model when relevant.

Never publish API keys, credentials, or sensitive code from logs or transcripts.

In a pull request, explain what changed, why, and how you tested it. Include
screenshots for visible TUI changes. Mention any checks you could not run.

## AI-assisted contributions

AI-assisted contributions—including changes made with cupel—are welcome.
You remain responsible for understanding, reviewing, and testing everything
you submit. Generated code must meet the same standards as any other contribution.

## License

Contributions are made under the project's [MIT license](LICENSE).
