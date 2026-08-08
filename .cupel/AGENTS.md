# Project

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. Architectural decisions are inspired by [pi.dev](https://pi.dev).

## Behavioral Guidelines

1. Think before Code: Don't assume. Don't hide confusion. Always Verfiy.
2. Simplicity First: Minimum code that solves the problem. Never speculative.
3. Surgical Changes: Touch only what you must. Clean up only your own mess.
4. Define Success Criteria: Transform tasks into verifiable goals.

## Coding Guidelines

* **Leverage the type system for correctness**: Use enums for state machines where variants are mutually exclusive. Prefer typestate patterns to make invalid states unrepresentable - methods only exist on valid state types.
* **Design tratis intentionally**: Use associated types when there's one natural impelementation per type; use generics when multiple implementations make sense. Keep traits object-safe when `dyn Trait` flexibility is needed (no -> `Self` returns, no generic methods).
* **Avoid bare .unwrap()**: Use combinators `.unwrap_or_default()`, `.unwrap_or_else(|| ...)`, or `.ok_or_else(...)` for recoverable cases. Reserve `.unwrap()` for test cases.
* **Prefer zero-cost abstraction**: Use iterator chaines over manual loops - they compile to equivalent code with better optimization opportunities. New types have no runtime overhead. Generics with trait bounds use static dispatch; reach for `dyn Trait` only when dynamic dispatch is genuinely needed.
* **Design errors intentionally**: Categorize errors by what callers can do (retry, skip, fail) rather than which component failed. Add meaningful context at module boundaries instead of blindly forwarding; ask "if this failes in production, what would I wish the log said?". Propagate with `?` and context via `.context()` and `.with_context()`.
* **Prefer `crate::` over `super::`**: Use absolute paths from the crate root for clarity and easier refactoring.
* **Use `pub use` sparingly**: Reserve re-exports for exposing dependencies so downstream consumers don't need direct dependencies—avoid it for internal module organization.
* **Avoid global state**: Skip `lazy_static!`, `OnceCell`, or similar patterns; prefer passing explicit context for shared state to keep dependencies visible and testing straightforward.
* **Use `LazyLock` for static regex**: Define regex patterns as `static PATTERN: LazyLock<Regex>` to compile once on first access. Use `.expect("reason")` in the initializer since pattern validty is known at write time.
* **Return `Option<T>` from validation checks**: Prefer returning `Option<Finding>` over booleans—callers use `if let Some(f) = check(...) { findings.push(f) }` to collect results cleanly.
* **Fluent builders with `#[must_use]`**: Configuration structs use `fn field(mut self, value: T) -> Self` methods marked `#[must_use]` to enable chaining while signaling that ignoring the return is likely a bug.
* **Layer configuration explicitly**: Apply settings in order—defaults first, then file config via `apply_file_config()`, then CLI overrides via `apply_cli_overrides()`. Each layer mutates a single config object.
* **Implement `FromStr` for enums**: Allow parsing strings to enum variants with descriptive error messages listing valid options. Use aliases (e.g., "ref" for MissingReference) to improve CLI ergonomics.
* **Borrow with explicit lifetimes for zero-copy**: When pairing results with source data, use lifetime parameters like `ValidationResult<'a>` holding `&'a Commit` to avoid cloning while keeping the borrow checker happy.

## Post-Implementation Hygiene

* Run `cargo fmt --check` or `cargo fmt` to either verifying or formatting Rust code accordingly.
* Use `cargo clippy` to run linter to catch common code mistakes and suggest improvements.
* Validate code changes by running `cargo test` or `cargo test <project file name>`.
* Compile the project in debug mode with `cargo build` or with a fast compile check with `cargo check`.
