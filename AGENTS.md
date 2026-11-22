# Project

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval.

## Behavioral Guidelines

1. Think before Code: Don't assume. Don't hide confusion. Always Verfiy.
2. Simplicity First: Minimum code that solves the problem. Never speculate.
3. Only test what you are really working on. Never exceed the scope.
3. Surgical Changes: Touch only what you must. Clean up only your own mess.
4. Define Success Criteria: Transform tasks into verifiable goals.

Before you make code changes, read the Rust coding guidelines [coding-guidelines.md](docs/coding-guidelines.md).

## Commit and Pull Request Guidelines

* Base new work on the latest fetched commit of the intended base branch.
* Review the staged diff before committing to catch unintended changes.
* Keep each commit focused on a single logical change and split larger changes into related groups.
* Keep commit messages and PR comments brief, clear, and focused on what changed.
* After merging a pull request, monitor the CI pipeline and fix any failures.

## Testing Scope

* Write tests only for new behavior, new features, regressions, compatibility exceptions, or non-obvious edge cases.
* Do not add a test that repeats coverage provided by an existing test or shared validation. Extend the existing test instead.
* Omit tests that merely restate input data, metadata, or an obvious success path; they add maintenance cost without protection.
* Prefer edge cases, boundaries, and behavior that is easy to get wrong or hard to infer from the implementation.

## Post-Implementation Hygiene

* Run `cargo fmt --check` or `cargo fmt` to either verifying or formatting Rust code accordingly.
* Use `cargo clippy` to run linter to catch common code mistakes and suggest improvements.
* Validate code changes by running `cargo test` or `cargo test <project file name>`.
* Compile the project in debug mode with `cargo build` or with a fast compile check with `cargo check`.
