---
name: no-comments
description: "Write code without explanatory comments and put the rationale in its final response, then check the diff before finishing."
disable: false
---

# No unnecessary code comments

## Write or edit code

Write code without adding explanatory comments. This includes inline and block comments, TODOs, doc comments, and docstrings. Use clear names, types, and direct control flow. Put the rationale for the change in the final response.

Add a comment only when the user explicitly asks for it or a verified legal, repository, or toolchain requirement calls for it. Keep required comments to the minimum required information. Preserve existing comments unless the edit makes them inaccurate or the user asks for comment cleanup.

## Review the requested scope

Use the caller's files or diff. Otherwise, review the diff against the change's target branch when it is known, including staged and unstaged edits. If the target branch is unknown, use the working-tree changes. Include new files that belong to the task. Keep review-only requests read-only.

- Flag comments that narrate the code, justify the edit, repeat names or types, or keep disabled code around. Review comment syntax in context so strings, fixtures, and examples are interpreted correctly.
- Verify what a comment does before removing it. Preserve legal notices, required tool directives, public API contracts, and verified external constraints that the code cannot express.
- Check diagnostic suppressions against the rule they disable. Flag broad suppressions and those hiding a real correctness issue. Preserve intentional expected-diagnostic tests. If removing a suppression exposes a failure, fix the cause within scope or preserve the suppression and report the issue.
- Trace constraints such as “do not remove” to the relevant code, test, or external contract. For a verified local invariant, use the smallest in-scope type, validation, test, or build check that enforces it. Preserve and report verified constraints that cannot be encoded within scope.

Remove redundant comments introduced by your edits. During requested cleanup, also remove redundant existing comments within scope. Review the final diff for new comments and accidental changes in behavior. Run the relevant checks if an edit affects tooling or behavior. Report any unresolved constraints or suppressions with their file and location.