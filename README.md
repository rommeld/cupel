# cupel

A cupel is a small vessel for refining precious metal. This project borrows that idea: separate useful code context from repository noise, then feed the refined signal into fast local agent workflows.

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. Search and reranking will lean on [fff](https://github.com/dmtrKovalenko/fff), with parts of the architecture inspired by [pi.dev](https://pi.dev) and parts shaped around my own coding workflows.

## Implementation milestones

### 1. `cupel-core`

The inference crate builds the foundation.

### 2. `cupel-agent`

Includes the basic agent definition and defines an agent loop primitive.

### 3. `cupel-coding-agent`

Use the `ripgrep` crate as the underlying for the **grep tool**. The crate also includes a simple `cuple CLI` to call functionality from the terminal. `ratatui` is the TUI crate of choice.
