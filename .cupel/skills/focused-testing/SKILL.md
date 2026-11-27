---
name: focused-testing
description: "Write minimal, focused, flake-free, parallel-safe tests and favor integration tests over unit-test bloat. Load before implementing, fixing bugs, refactoring, or editing tests, even when the request omits tests."
---

# Focused Testing

Tests are permanent maintenance cost. Default to zero new tests; every test must earn its keep by catching a real failure existing checks miss.

## Decide before writing

- Inspect existing tests, fixtures, and CI commands first.
- Add no test when existing tests, types, linters, or builds already catch the failure. For mechanical changes, zero new tests is valid.
- Extend a nearby test before adding a new one. Delete redundant or obsolete tests.
- While coding, keep external boundaries injectable and avoid mutable process globals so integration and parallel tests stay clean.

## Test boundary and focus

- For multi-layer behavior, write one focused integration test through the normal entry point. Exercise real internal layers; stub only true external boundaries (network, clock, randomness, third-party APIs). Use disposable real dependencies (DB, filesystem) when their semantics matter.
- Never mirror new functions with 1:1 unit tests. Skip unit tests for wrappers, plain data types, trivial wiring, or behavior already covered by an integration test.
- Reserve unit tests for non-obvious isolated logic (algorithms, parsers, complex rules, edge math) that is awkward or slow to cover via integration.
- Give each test one behavioral purpose, minimal setup, and observable assertions (outputs, state, side effects). Parameterize variants of the same rule.

## Regressions

- On every bug fix, extend an existing test or add one focused regression test for the triggering condition.
- Confirm the test fails on unfixed code for the expected reason, then passes after the fix. Report explicitly if you could not verify the failure first.

## Parallel-safe and flake-free

- Every added or changed test must pass alone, in any order, and in parallel.
- Isolate state per test: unique temp dirs, ephemeral ports, isolated DB state, and cleanup scoped to resources the test created, even on failure.
- Never mutate process-global state (env vars, working directory, singletons, module globals) in parallel tests. Isolate unavoidable process-wide state in a subprocess, or report the constraint instead of silently serializing.
- Keep tests deterministic: control clocks and randomness, avoid ordering assumptions, and use explicit waits or synchronization. Never use fixed sleeps or retries to hide races.
- Run affected tests under the project's parallel test setup when available, and report if parallel execution could not be verified.

## Never hide flaws

- When a valid test fails, fix the code, not the test.
- Never weaken assertions, assert only coarse success when data or side effects matter, skip failing tests, or blindly update snapshots.
- Never bypass the real path, mock broken internals, add test-only branches to production code, or copy buggy output into expectations. Update expectations only when the intended contract changes.
