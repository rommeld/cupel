# cupel

A cupel is a small vessel for refining precious metal. This project borrows that idea: separate useful code context from repository noise, then feed the refined signal into fast local agent workflows.

`cupel` is a lean Rust coding harness focused on provider-neutral inference, deterministic tooling, CLI/TUI workflows, and efficient code retrieval. Search and reranking will lean on [fff](https://github.com/dmtrKovalenko/fff), with parts of the architecture inspired by [pi.dev](https://pi.dev) and parts shaped around my own coding workflows.

## Current structure

`cupel-inference`: a provider-neutral inference crate that owns model metadata, request/context types, streaming assistant events, usage/cost types, tool-call shapes, provider registries, and protocol adapters.

Already in place:

- provider-neutral `InferenceClient`, `InferenceProvider`, `InferenceRequest`, `InferenceContext`, and streaming `AssistantMessageEvent` types
- model/provider registries keyed by `ModelRef` and `ApiFamily`
- OpenAI-compatible chat completions adapter (**not finished**)
- OpenAI Responses adapter in progress (**not finished**)
- feature flags reserved for Anthropic, Google, Mistral, and Bedrock adapters

## Plan

### 1. Expand `cupel-inference`

The inference crate builds the foundation.

- [ ] Finish and harden the OpenAI Responses adapter
  - [ ] complete request field mapping, including output token limits, reasoning options, tools, usage, and finish reasons
  - [ ] finalize streamed tool-call accumulation into `AssistantMessage.tool_calls`
  - [ ] add fixture-based stream tests for text, reasoning, tool calls, usage, errors, and malformed JSON
- [ ] Add a Codex provider adapter
  - [ ] define which API family and wire protocol it uses
  - [ ] map Codex-specific streaming events into the provider-neutral event model
  - [ ] keep auth/config injection outside the provider, matching the current crate boundary
  - [ ] cover cancellation, raw-event debugging, usage, and tool-call behavior with tests
- [ ] Add an Anthropic Messages adapter
  - [ ] map system/user/assistant/tool messages into Anthropic's request shape
  - [ ] map streamed content blocks, thinking/reasoning, tool-use deltas, usage, and stop reasons back into cupel events
  - [ ] decide how Anthropic thinking budgets map onto `ReasoningEffort` or whether `InferenceRequestOptions.extra` remains the first escape hatch
- [ ] Add registry bootstrap helpers
  - [ ] create model/provider registration presets that are useful to the CLI without making inference read environment variables
  - [ ] keep provider registration feature-gated and deterministic for tests
- [ ] Tighten the public API
  - [ ] document crate boundaries
  - [ ] remove obvious typos and naming drift
  - [ ] keep provider errors structured enough for the runtime and TUI to display well

### 2. Add `cupel-cli`

The CLI will be a thin shell around config, provider selection, session startup, and display.

- [ ] Add a workspace crate for the CLI
- [ ] Load config, secrets, model presets, and workspace settings outside `cupel-inference`
- [ ] Support direct one-shot prompts for smoke testing providers
- [ ] Add model/provider inspection commands
- [ ] Add a TUI for one active session
  - [ ] render streaming text, reasoning summaries, tool calls, tool results, usage, and errors
  - [ ] expose model/provider selection without hiding the raw IDs
  - [ ] keep keyboard flow fast enough for local coding loops

### 3. Add `cupel-agent`

The runtime coordinates the loop, own mutable session state, and call tools. Providers should continue to only produce assistant events.

- [ ] Add a long-lived agent runtime crate
- [ ] Maintain append-only session history and a mutable working context
- [ ] Execute tool calls through a typed tool registry
- [ ] Feed tool results back into `InferenceContext`
- [ ] Track budget, token usage, and provider cost across turns
- [ ] Add interruption, resume, and failure recovery points
- [ ] Define context handoff/compaction before sessions become too large

### 4. Add Search and Rerank

Search will be built as an agent tool, not as a pile of shell fallbacks. The target is a fast [`fff`](https://github.com/dmtrKovalenko/fff)-backed retrieval layer with a ranking/presentation pass inspired by [Entire's agentic-search study](https://entire.io/blog/improving-agentic-search-in-coding-agents).

- [ ] Add a search crate or runtime service around `fff`
  - [ ] keep a long-lived repository index warm
  - [ ] use path search, content search, frecency, git status, and definition hints
  - [ ] expose stable tool contracts such as `find_files`, `search_code`, and `read_code`
- [ ] Add a rerank layer above raw matches
  - [ ] rank definitions and likely entry points first
  - [ ] prefer source files over tests, generated files, vendored code, and build output unless the query asks otherwise
  - [ ] include path-aware, symbol-aware, git-aware, and recency/frecency signals
  - [ ] group and trim output so the agent sees the best next files quickly
  - [ ] preserve enough score/debug data to tune the ranking
- [ ] Evaluate retrieval directly before optimizing the full agent loop
  - [ ] replay first-search and pre-read queries
  - [ ] measure MRR, Hit@1, Hit@3, and output size
  - [ ] compare raw `rg`, raw `fff`, and cupel rerank on the same tool contract
  - [ ] only then run end-to-end agent tasks for wall-clock, cost, tool-call count, and successful completion
