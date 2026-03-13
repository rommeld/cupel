---
name: issue_writer
description: >
  Generate structured, type-aware issue markdown files from task descriptions.
  Use this skill whenever the user asks to create issues, tickets, tasks, bug reports,
  feature requests, or any project-tracking artifact written as markdown. Also trigger
  when the user says things like "write me an issue for…", "break this work down",
  "I need to track this task", "document this bug", "create a ticket", or describes
  work that needs scoping, splitting, or formal specification. The skill covers scope
  assessment, issue-type classification, template selection, test guidance, and
  Definition of Done — and writes all output into a `.issues/` directory at the
  project root.
---

# Issue Writer

Write comprehensive, agent-readable issue markdown files from raw task descriptions.
Every issue is self-contained: an agent (or human) picking it up should understand
the problem, the shape of the solution, how to verify it, and when it is done —
without reading anything else.

---

## 1 — Output location and naming

Store every generated issue file inside a `.issues/` directory at the **root** of the
project (create it if it does not exist).

### Naming convention

```
.issues/<type>-<slug>.md
```

- `<type>` is one of: `bug`, `feature`, `refactor`, `debt`, `docs`, `perf`.
- `<slug>` is a lowercase, hyphen-separated summary (3–6 words max).

**Examples:**

```
.issues/bug-null-pointer-on-empty-input.md
.issues/feature-user-export-csv.md
.issues/refactor-extract-auth-middleware.md
```

When a task is split into a parent with children, create:

```
.issues/feature-payment-flow.md              ← parent
.issues/feature-payment-flow--api-design.md  ← child
.issues/feature-payment-flow--ui-form.md     ← child
.issues/bug-payment-flow--rounding-error.md  ← child (different type is OK)
```

The double-hyphen `--` separates the parent slug from the child slug. Children
reference the parent file path in their front matter.

---

## 2 — Scope assessment: single issue or split?

Before writing anything, evaluate scope. The goal is that each issue represents a
single reviewable, testable, deployable unit of work.

### Heuristics for splitting

Consider splitting when **two or more** of these are true:

| Signal                        | Indicator                                                              |
| ----------------------------- | ---------------------------------------------------------------------- |
| **Distinct concerns**         | The task touches unrelated subsystems (e.g., API + UI + DB migration). |
| **Multiple actors**           | Different people or teams would naturally own different parts.         |
| **Independent deployability** | Parts could ship and deliver value on their own.                       |
| **Estimated effort**          | Total effort exceeds roughly 1–2 working days for one person.          |
| **Dependency chain**          | One part blocks another — explicit ordering helps.                     |
| **Mixed types**               | The task blends a bug fix with a new feature or refactoring.           |

If only zero or one signal fires, keep it as a single issue.

### When you split

1. Create a **parent issue** that describes the overall objective and lists every
   child with its file path.
2. Create one **child issue per concern**. Each child carries its own type, template,
   tests, and Definition of Done.
3. In the parent, include a section called `## Child Issues` with a checklist:
   ```markdown
   ## Child Issues

   - [ ] [API design](feature-payment-flow--api-design.md)
   - [ ] [UI form](feature-payment-flow--ui-form.md)
   - [ ] [Fix rounding](bug-payment-flow--rounding-error.md)
   ```
4. In each child, add front-matter or a header line:
   ```markdown
   **Parent:** [Payment Flow](feature-payment-flow.md)
   ```

---

## 3 — Classifying the issue type

Determine the type from the task description before selecting a template. This
table maps common language cues to issue types:

| Type         | Typical cues in the description                                                                                         |
| ------------ | ----------------------------------------------------------------------------------------------------------------------- |
| **bug**      | "broken", "crash", "incorrect output", "regression", "doesn't work", error messages, stack traces, unexpected behaviour |
| **feature**  | "add", "new capability", "user should be able to", "support for", "as a user I want"                                    |
| **refactor** | "clean up", "restructure", "extract", "rename", "simplify", "reduce coupling" — no observable behaviour change          |
| **debt**     | "upgrade dependency", "remove deprecated API", "fix linter warnings", "address TODOs", "migrate to new library"         |
| **docs**     | "document", "README", "API reference", "add examples", "onboarding guide"                                               |
| **perf**     | "slow", "timeout", "optimise", "reduce memory", "latency", benchmark numbers                                            |

### Disambiguation rules for ambiguous tasks

Some tasks straddle types. Apply these tie-breakers in order:

1. **Behaviour is currently wrong** → `bug`, even if the fix also improves code
   structure.
2. **External behaviour changes for the user** → `feature`, even if it also
   cleans things up internally.
3. **Performance is the primary goal** → `perf`, even if the approach is
   restructuring code.
4. **No external behaviour changes at all** → `refactor` if the code is being
   reshaped, `debt` if the change is about maintenance hygiene (dependency
   bumps, warning suppression, config migration).
5. **The deliverable is prose, not code** → `docs`.

If genuinely uncertain after these steps, pick the type whose template captures
the most useful information for the task and note the ambiguity in the issue body
under a `> **Classification note:**` blockquote.

---

## 4 — Templates by issue type

Every issue begins with a common header, then diverges by type.

### 4.0 — Common header (all types)

```markdown
# <Title>

| Field        | Value         |
| ------------ | ------------- |
| **Type**     | `<type>`      |
| **Priority** | `<p0–p3>`     |
| **Parent**   | (path or "—") |
| **Created**  | <YYYY-MM-DD>  |

## Summary

A 1–3 sentence plain-language description of what this issue is about and why it
matters.
```

Priority levels:

- **p0** — System down or data loss; drop everything.
- **p1** — Significant impact; address within the current sprint.
- **p2** — Important but not urgent; schedule soon.
- **p3** — Nice-to-have; backlog.

---

### 4.1 — Bug

```markdown
## Environment

Runtime, OS, language version, relevant dependency versions.

## Steps to Reproduce

1. …
2. …
3. …

## Expected Behaviour

What should happen.

## Actual Behaviour

What happens instead. Include error messages, logs, or screenshots if available.

## Root Cause Analysis

If the cause is known or suspected, describe it here. Otherwise write
"To be investigated."

## Proposed Fix

High-level approach to the fix (not a full implementation plan).
```

---

### 4.2 — Feature

```markdown
## User Story

As a <role>, I want <capability>, so that <benefit>.

## Detailed Description

Expand on the user story: context, workflows affected, edge cases to consider.

## Success Criteria

- [ ] Criterion 1 — observable, verifiable outcome.
- [ ] Criterion 2
- …

## Design Considerations

API contracts, UI wireframe notes, data model changes, or architectural
decisions. Link to external design docs if they exist.

## Out of Scope

Explicitly list things this issue does NOT cover to prevent scope creep.
```

---

### 4.3 — Refactor

```markdown
## Current State

Describe the code or architecture as it exists today and why it is problematic.

## Target State

Describe the desired structure after the refactor.

## Migration Strategy

Step-by-step approach to move from current to target without breaking things.
Favour incremental steps that keep the build green.

## Behavioural Invariants

List the external behaviours that must remain unchanged after this refactor.
These become your regression-test anchors.
```

---

### 4.4 — Technical Debt

```markdown
## Debt Description

What the debt is and how it accumulated.

## Impact

Concrete consequences of leaving this debt in place (slower builds, security
exposure, blocked upgrades, developer friction).

## Remediation Plan

Ordered steps to pay down the debt.

## Rollback Considerations

What to do if the remediation causes unexpected problems.
```

---

### 4.5 — Documentation

```markdown
## Audience

Who will read this documentation (end-users, developers, ops, etc.).

## Scope

Which topics or APIs the documentation covers.

## Outline

Proposed structure / table of contents for the documentation deliverable.

## Source Material

Links to code, existing docs, conversations, or specs that inform the content.
```

---

### 4.6 — Performance

```markdown
## Current Performance

Measurements or observations: latency, throughput, memory, CPU, bundle size.
Include how measurements were taken (tool, environment, dataset).

## Target Performance

Quantitative targets that define success (e.g., "p99 latency < 200ms under 1k
concurrent requests").

## Hypothesis

What you believe is causing the bottleneck and why.

## Proposed Approach

Optimisation strategy — algorithmic change, caching, parallelism, resource
pooling, etc.

## Benchmark Plan

How performance will be measured before and after to validate improvement.
```

---

## 5 — Test definition

Every issue includes a `## Test Guidance` section. The content varies by type —
the aim is to tell the implementer _what kinds_ of tests to write, not to supply
code.

### Principles

- Describe the **category** of test and what it should exercise, not the
  implementation.
- Be specific about **boundary conditions** and **critical paths** worth testing.
- Mention any infrastructure the tests will need (fixtures, mocks, staging
  environment, seed data).

### Type-specific guidance

| Type         | Primary test category         | Focus areas                                                                                                                           |
| ------------ | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| **bug**      | Unit tests                    | Reproduce the exact failing case first. Cover surrounding edge cases that share the same code path.                                   |
| **feature**  | Integration tests             | Verify end-to-end flows through the new capability. Cover happy path, key error paths, and permission boundaries.                     |
| **refactor** | Regression tests              | Existing tests should continue to pass unchanged. If coverage gaps exist around the refactored area, add unit tests before starting.  |
| **debt**     | Compatibility tests           | Verify that upgraded dependencies or migrated configs still produce identical behaviour in CI.                                        |
| **docs**     | Review checklist              | Docs should be technically accurate (code samples run), internally consistent, and pass any linter or link-checker in the project.    |
| **perf**     | Performance / benchmark tests | Before-and-after benchmark under controlled conditions. Record environment, dataset, and iteration count so results are reproducible. |

Write the `## Test Guidance` section using this structure:

```markdown
## Test Guidance

**Primary category:** <category from table>

**What to test:**

- <specific behaviour or scenario 1>
- <specific behaviour or scenario 2>

**Boundary conditions:**

- <edge case worth covering>

**Infrastructure needs:**

- <any fixtures, mocks, environments, or data required>
```

---

## 6 — Definition of Done

Every issue includes a `## Definition of Done` section. Start with a shared
baseline, then append type-specific gates.

### Shared baseline (always include)

```markdown
## Definition of Done

- [ ] Implementation is complete and builds without errors.
- [ ] All new and existing tests pass in CI.
- [ ] Code has been reviewed and approved by at least one other engineer.
- [ ] Changes are documented — at minimum, meaningful commit messages and
      inline comments where intent is non-obvious.
```

### Type-specific gates

Append the relevant extras below the baseline checklist:

**bug**

```markdown
- [ ] A test that reproduces the original bug is included and passes.
- [ ] Regression has been verified in an environment matching the reporter's setup.
```

**feature**

```markdown
- [ ] All success criteria listed in the issue are met.
- [ ] User-facing documentation or changelog entry is updated.
- [ ] Feature flag or rollout strategy is in place (if applicable).
```

**refactor**

```markdown
- [ ] All behavioural invariants listed in the issue still hold.
- [ ] No new warnings or linter violations introduced.
```

**debt**

```markdown
- [ ] The targeted debt item is fully resolved (dependency upgraded, deprecation
      removed, etc.).
- [ ] Rollback path has been tested or documented.
```

**docs**

```markdown
- [ ] Documentation has been reviewed for technical accuracy by a domain owner.
- [ ] All code samples have been tested and run successfully.
- [ ] Links are valid and not broken.
```

**perf**

```markdown
- [ ] Performance targets defined in the issue are met or exceeded.
- [ ] Benchmark results are recorded and attached to the issue or PR.
- [ ] No functional regressions introduced by the optimisation.
```

---

## 7 — Generation workflow (step by step)

When asked to create an issue, follow these steps in order:

1. **Parse the task description.** Extract the core intent, affected components,
   and any constraints the user mentioned.
2. **Classify the type** using the cue table and disambiguation rules in §3.
3. **Assess scope** using the heuristics in §2. Decide: single issue or split.
4. **Select the template** from §4 matching the classified type.
5. **Fill in every section.** Do not leave placeholders like "TBD" — if information
   is missing, state what is unknown and what assumptions you are making.
6. **Write the Test Guidance** section per §5.
7. **Write the Definition of Done** section per §6.
8. **Name the file** per the conventions in §1 and write it to `.issues/`.
9. **If splitting**, create the parent first, then each child. Ensure cross-links
   are correct.
10. **Present the file(s)** to the user for review.

---

## 8 — Full single-issue example

Below is a condensed example to show how the pieces fit together. Real issues
should be more thorough; this is for structural illustration.

```markdown
# CSV export crashes on empty filter result

| Field        | Value      |
| ------------ | ---------- |
| **Type**     | `bug`      |
| **Priority** | `p1`       |
| **Parent**   | —          |
| **Created**  | 2026-03-12 |

## Summary

Exporting a filtered dataset to CSV throws an unhandled `IndexError` when the
filter returns zero rows. Users see a 500 page instead of an empty file.

## Environment

- Python 3.12, Django 5.1, Pandas 2.2
- Reproduced on staging (Linux/Docker) and locally (macOS 15).

## Steps to Reproduce

1. Navigate to /reports and apply a date filter for a range with no data.
2. Click "Export CSV".
3. Observe the 500 error.

## Expected Behaviour

The server responds with a valid CSV file containing only the header row.

## Actual Behaviour

`IndexError: single positional indexer is out-of-bounds` in
`export_service.py:42`. Full traceback is in the attached log.

## Root Cause Analysis

`export_csv()` calls `df.iloc[0]` to sniff column types before writing.
When the dataframe is empty this raises `IndexError`.

## Proposed Fix

Guard with an `if df.empty` check and short-circuit to writing headers only.

## Test Guidance

**Primary category:** Unit tests

**What to test:**

- Exporting with zero rows returns a CSV containing only the header.
- Exporting with one row and many rows still works as before.

**Boundary conditions:**

- DataFrame with columns but zero rows.
- DataFrame with zero columns (unlikely but defensive).

**Infrastructure needs:**

- Existing `ReportFactory` test fixture generates sample data; extend it to
  produce an empty-result filter scenario.

## Definition of Done

- [ ] Implementation is complete and builds without errors.
- [ ] All new and existing tests pass in CI.
- [ ] Code has been reviewed and approved by at least one other engineer.
- [ ] Changes are documented — at minimum, meaningful commit messages and
      inline comments where intent is non-obvious.
- [ ] A test that reproduces the original bug is included and passes.
- [ ] Regression has been verified in an environment matching the reporter's
      setup.
```

---

## 9 — Full split-issue example (parent)

```markdown
# Payment flow redesign

| Field        | Value      |
| ------------ | ---------- |
| **Type**     | `feature`  |
| **Priority** | `p1`       |
| **Parent**   | —          |
| **Created**  | 2026-03-12 |

## Summary

Replace the legacy single-step payment page with a multi-step checkout flow
that supports saved payment methods and real-time validation.

## Child Issues

- [ ] [API design](feature-payment-flow--api-design.md)
- [ ] [UI multi-step form](feature-payment-flow--ui-form.md)
- [ ] [Fix rounding error in tax calc](bug-payment-flow--rounding-error.md)

## User Story

As a customer, I want a guided checkout flow so that I can complete purchases
with fewer errors and the option to reuse saved cards.

## Success Criteria

- [ ] Checkout converts at least as well as the current single-step page.
- [ ] Saved cards are usable without re-entering details.
- [ ] Tax is calculated correctly to the cent for all supported locales.

## Out of Scope

- Subscription / recurring billing (tracked separately).
- Gift card support.
```

Each child carries its own full template, Test Guidance, and Definition of Done.
