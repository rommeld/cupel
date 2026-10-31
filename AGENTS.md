# Project

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval.

## Behavioral Guidelines

1. Think before Code: Don't assume. Don't hide confusion. Always Verfiy.
2. Simplicity First: Minimum code that solves the problem. Never speculative.
3. Surgical Changes: Touch only what you must. Clean up only your own mess.
4. Define Success Criteria: Transform tasks into verifiable goals.

For Rust coding guidelines, read and follow [RUST-CODING-GUIDE.md](RUST-CODING-GUIDE.md).

## Post-Implementation Hygiene

* Run `cargo fmt --check` or `cargo fmt` to either verifying or formatting Rust code accordingly.
* Use `cargo clippy` to run linter to catch common code mistakes and suggest improvements.
* Validate code changes by running `cargo test` or `cargo test <project file name>`.
* Compile the project in debug mode with `cargo build` or with a fast compile check with `cargo check`.
