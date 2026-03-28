# AI Agent Context

This file provides guidance to all AI code assistants when working with code in this repository.

# Project

This project is a cellar book for managing wine collections in private or professional settings. It documents information such as quantity, grape variety, producer, region, country, vintage, special characteristics, price, and other relevant details. The current focus is on iOS ecosystem. The application is written in Rust and Swift.

## Technology Stack

### Backend (Rust)

- **gRPC**: tonic
- **Error Handling**: thiserror + anyhow
- **Async Runtime**: tokio
- **Serialization**: serde + serde_json

### iOS App (Swift)

- **UI Framework**: SwiftUI
- **Layout**: SwiftUI native (with occasional UIKit via UIViewRepresentable)
- **Architecture**: MVVM with Observable macro
- **Networking**: URLSession with async/await
- **Data Persistence**: SQLite.swift
- **Dependency Injection**: Environment values

## Commands

### Rust (Backend)

| Command                                        | Purpose                                                          |
| ---------------------------------------------- | ---------------------------------------------------------------- |
| `cargo build`                                  | Compile the project in debug mode                                |
| `cargo build --release`                        | Compile with optimizations for production                        |
| `cargo run`                                    | Build and execute the binary                                     |
| `cargo test`                                   | Run all unit tests, integration tests, and doc-tests             |
| `cargo test <name>`                            | Run tests matching a specific name or pattern                    |
| `cargo test -- package::module::function_name` | Run a single test by full path                                   |
| `cargo clippy`                                 | Run the linter to catch common mistakes and suggest improvements |
| `cargo clippy -- -D warnings`                  | Treat warnings as errors (useful in CI)                          |
| `cargo fmt`                                    | Format code according to Rust style guidelines                   |
| `cargo fmt --check`                            | Verify formatting without modifying files (useful in CI)         |
| `cargo doc --open`                             | Generate and open documentation for the project and dependencies |
| `cargo update`                                 | Update dependencies to newest compatible versions per Cargo.toml |
| `cargo check`                                  | Fast compile check without producing binaries—useful during dev  |

### Swift (iOS App)

| Command                                              | Purpose                                      |
| ---------------------------------------------------- | -------------------------------------------- |
| `xcodebuild -project Cupel.xcodeproj -scheme Cupel build` | Build the iOS project                   |
| `xcodebuild test -project Cupel.xcodeproj -scheme Cupel` | Run iOS unit tests                      |
| `swift build`                                        | Build Swift Package Manager targets          |
| `swift test`                                        | Run Swift tests                              |

# Code Style

## Imports

- **Prefer absolute paths** (`crate::module::Item`) over relative (`super::Item` or `self::Item`)
- **Group imports** by crate: std → external → crate (use std::_, use crate::_, etc.)
- Order: standard library → external crates → local crate modules
- Avoid re-exports (`pub use`) except for exposing dependencies downstream consumers need

## Naming Conventions

- **Types/Enums**: PascalCase (`GitFileStatus`, `RepoPath`)
- **Functions/Methods**: snake_case (`open()`, `discover()`, `parse_numstat()`)
- **Struct Fields**: snake_case (`work_directory`, `index_status`)
- **Constants**: SCREAMING_SNAKE_CASE or PascalCase for newtypes
- **Traits**: PascalCase with `Trait` suffix when ambiguous (`Clone`, not `Clonable`)
- **Modules**: snake_case (`git`, `diff`, `ui`)
- **Files**: snake_case (`real_repo.rs`, `types.rs`)

## Type System

- **Leverage the type system for correctness**: Use enums for state machines where variants are mutually exclusive
- **Use newtype pattern**: `struct RepoPath(Arc<Path>)` to enforce semantic distinctions at compile time
- **Prefer typestate patterns**: Make invalid states unrepresentable—methods only exist on valid state types
- **Prefer Copy/Clone intentionally**: Add `#[derive(Copy, Clone)]` only when meaningful; avoid for large types

## Traits

- Use associated types when there's one natural implementation per type
- Use generics when multiple implementations make sense
- Keep traits object-safe when `dyn Trait` flexibility is needed (no `-> Self` returns, no generic methods)
- Implement common traits (`Debug`, `Clone`, `PartialEq`, `Default`) consistently

## Error Handling

- Use **thiserror** for domain-specific errors with variants callers may need to match (e.g., `GitError`)
- Use **anyhow** for application-level errors where context matters more than variant matching
- Error messages: lowercase, no trailing punctuation, describe only the immediate problem
- Let the error source chain convey causality—propagate with `?`
- Add meaningful context at module boundaries: `.context("opening repository")` or `.with_context(|| ...)`
- Prefer preserving error chains over formatting errors inline

## Unwrap/Expect

- **Avoid bare `unwrap()`** in production code
- Use `expect("reason")` when behavior is predictable and failure indicates a bug
- Use combinators for recoverable cases: `unwrap_or_default()`, `unwrap_or_else(|| ...)`, `ok_or_else(|| ...)`
- Reserve bare `unwrap()` only for tests

## Abstractions

- **Prefer zero-cost abstractions**: Iterator chains compile to equivalent manual loops
- Newtypes have no runtime overhead—use them freely for semantic distinction
- Generics with trait bounds use static dispatch; reach for `dyn Trait` only when dynamic dispatch is genuinely needed

## Documentation

- Document **# Panics**, **# Errors**, and **# Safety** sections where applicable
- Write doc-tests in `///` comments to keep examples synchronized with code
- Prefer clear, obvious code over documenting the obvious

## Testing

- Place unit tests in `#[cfg(test)]` modules at the end of the file (see `src/git/types.rs` for examples)
- Use descriptive test names: `staging_state_fully_staged` not `test1`
- Test both positive and negative cases
- Use `tempfile` crate for tests that need temporary directories

## Git/CLI Integration

- Use `git2` crate for local git operations (reading commits, status, etc.)
- Use `std::process::Command` for git CLI when needed (network operations, complex output)
- Parse CLI output carefully—handle binary files, malformed lines gracefully (see `parse_numstat`)

## Serialization (serde)

- Use `#[serde(rename_all = "snake_case")]` or `#[serde(rename_all = "SCREAMING_SNAKE_CASE")]` for JSON field mapping
- Prefer `#[derive(Serialize, Deserialize)]` with explicit field names over custom implementations
- Add `#[serde(default)]` for optional fields that should default to zero/empty values

## Additional Guidelines

### Rust

- **Keep dependencies current**: Regularly update to newest crate versions
- **Avoid global state**: Skip `lazy_static!`, `OnceCell`; prefer explicit context passing
- **Format code before committing**: Always run `cargo fmt`
- **Run linter before submitting**: Always run `cargo clippy`
- **Never disable tests**: Fix them instead
- **Never commit code that doesn't compile**: Ensure `cargo build` succeeds
- **Commit incrementally**: Small, working changes over big bang PRs
- **Text files**: Always end with an empty line
- **Avoid clever tricks**: Choose the boring, obvious solution—if you need to explain it, it's too complex
- **Single responsibility**: Each function/module should do one thing well
- **Interface over singletons**: Enable testing and flexibility through dependency injection
- **Fail fast**: Include descriptive error messages for debugging

### Swift

- **Swift Style Guide**: Follow [Apple's Swift API Design Guidelines](https://www.swift.org/documentation/api-design-guidelines/)
- **Type Names**: PascalCase (`WineCellar`, `BottleDetails`)
- **Function/Method Names**: camelCase (`fetchBottles()`, `addBottle()`)
- **Variables/Constants**: camelCase (`wineCollection`, `bottleCount`)
- **Enums**: PascalCase with associated values on separate lines
- **Prefer `struct`s over `class`es**: Swift structs have automatic initializers and value semantics
- **Use `guard` for early returns**: Keeps code flat and readable
- **Avoid `self` where not needed**: Swift allows implicit references in closures and methods
- **Use `Observable` macro over `ObservableObject`**: For iOS 17+ with `@Observable` property wrapper
- **Prefer `@State` and `@Bindable` for local state**: Use environment values for shared state
- **Use `async/await` for asynchronous operations**: Avoid completion handlers where possible
- **Never force unwrap**: Use `guard let` or `if let` for optionals
- **Text files**: Always end with an empty line
