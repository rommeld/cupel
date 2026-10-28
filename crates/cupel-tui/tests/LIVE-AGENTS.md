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

## Extension checkpoint

The baseline result does **not** establish M10 shared discussion, M11 recursive
scheduling, or M12 discovered-role routing. Their automated tests and the expanded
live check must be recorded separately after those milestones are implemented.
