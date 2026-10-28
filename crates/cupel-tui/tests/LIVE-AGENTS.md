# Multi-agent verification

## M8 baseline — 2026-10-28

Verified against real Fireworks inference using an isolated Cupel home and a
disposable Git checkout, not mocks. No existing settings or project files were
changed. Agent names in the fixtures are examples, not built-ins.

Models: `accounts/fireworks/models/minimax-m3p1-flash` for the lead and reader;
`accounts/fireworks/models/minimax-m3` for the reviewer/writer. Only Fireworks was
selected for this run; this does not verify other providers' cache behavior.

The live application test passed:

- Lazy startup, invalid-model warnings, and recovery through hot reload.
- Direct helper execution, answers/progress, usage, and lead exchange awareness.
- One-shot arming and Esc disarming.
- Real model-initiated `ask` and writer `delegate` calls.
- Actual edits in the disposable checkout.
- Esc cancellation of a sleeping delegated shell while the lead continued.
- Explicit `/review --diff` routing without a lead turn.
- Persisted helper transcripts, lazy resume, remembered helper history, restored
  membership, and restored helper spend.

Reported combined usage was 12,926 input / 3,205 output / 35,002 cached tokens,
approximately **$0.007300**. Costs and caching vary between runs. An additional
small billable terminal probe verified the real binary's armed input title,
Esc notice, helper completion, persisted exchange, normal quit, and restoration
of terminal settings and the alternate screen.

Reproduce explicitly (these commands make billable calls):

```sh
CUPEL_LIVE_AGENTS=1 cargo test -p cupel-tui --test live_agents -- --ignored --nocapture
cargo build -p cupel-tui --bin cupel
CUPEL_LIVE_AGENTS=1 python3 crates/cupel-tui/tests/support/live_agents.py target/debug/cupel
```

Both use the Fireworks key in the existing Cupel home; they never print it.
The application test is ignored in normal CI. The PTY check requires Unix and
Python's standard library. Provider output is nondeterministic: an assertion
failure must be inspected, not treated as proof of a routing defect by itself.

## M10–M12 expanded checkpoint — 2026-10-28

The complete baseline plus expanded application test passed against Fireworks
after implementing M10–M12 (80.93 seconds). It additionally verified:

- Explained role hints without constructing helpers; inferred `/review` and the
  typed startup review handler without an explicit mapping.
- A writer asking a read-only reviewer by role, both publishing attributed
  discussion posts, followed by an integrated answer visible to the lead.
- Two concurrent real-provider calls queuing for the same busy specialist.
- Cancellation by nested task ID followed by Esc, with the sleeping shell's
  process group gone, no late write, and partial changed paths retained. The
  fixture checks cancellation completes within 15 seconds, not after its
  90-second sleep. Its event driver includes a timer wakeup like the real TUI.
- Ambiguous reviewers falling back to the lead; an invalid explicit mapping
  never selecting another discovered helper.
- Thread/task/spend retention through reload, removed members remaining removed,
  and fresh-session discussion isolation without an initial transcript write.

The successful full run reported **45,790 input / 14,565 output / 86,159 cached**
tokens, approximately **$0.029760**; helper usage was attributed to 15 calls.
The real-terminal probe also passed again after these extensions.

Cycle refusal, hard request/depth/call/deadline limits, interrupted-task recovery,
and read-only access ceilings are tested deterministically with mock providers
and scheduler tests rather than relying on a model to provoke failures.

The live pass exposed and fixed interleaved discussion updates splitting streamed
answers or hiding final-answer promotion. A deterministic regression test covers
that behavior. The fixture also uses a 4096-token output ceiling, above MiniMax's
low thinking budget; a 1024-token ceiling was rejected by the provider. It uses
fresh writer history for independent extension scenarios so earlier cancellation
instructions do not contaminate later tasks. These are provider/model-dependent
checks, not a guarantee that arbitrary models always obey delegation prompts.

To run just the expanded scenarios while iterating:

```sh
CUPEL_LIVE_AGENTS=1 CUPEL_LIVE_EXTENDED_ONLY=1 cargo test -p cupel-tui --test live_agents -- --ignored --nocapture
```
