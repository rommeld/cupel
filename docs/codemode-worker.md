# Codemode preview worker

The `codemode-preview` Cargo feature builds two private workers into `cupel`
and `cupel-acp`. Each frontend starts its own executable as a worker.
The Monty worker uses a cleared environment and `/` as its working directory.
Monty receives no filesystem mounts. The worker rejects all Monty OS calls and
forwards only `read` and `grep` requests. It does not execute those requests.

The session broker checks each callback against the same hooks as a direct
call. Only after preflight does it start a file worker. That worker resolves
paths, reads text, walks directories, runs searches, and reads grep context.
It has the same file permissions as Cupel and does not confine paths to the
checkout. The parent owns the instruction snapshot and records observations
from the identities and digests of the bytes that supplied each view.

The build feature alone does not register the tool. See the
[codemode preview guide](codemode-preview.md) for opt-in and offline checks.

| Limit | Preview budget |
| --- | --- |
| Active scripts | 1 for the plain preview flag; 2 for home opt-in, per process |
| Script source | 16 KiB of UTF-8 |
| Guest memory | 16 MiB above Monty's allocator baseline |
| Hard allocator ceiling | 20 MiB above the worker's initial allocator baseline |
| Host calls | 16 sequential `read` or `grep` calls |
| Callback arguments | 16 KiB of JSON per request |
| Callback results | 256 KiB cumulative JSON |
| Emits | 16 KiB cumulative JSON |
| Guest execution | 1 second, excluding worker startup and time awaiting host callbacks |
| Call deadline | 5 seconds, including admission, startup, callbacks, and cleanup |
| Work cutoff | 4 seconds after the call starts, including time in the queue |

Only child processes arm hard allocator limits. The Monty worker's 4 MiB of
headroom allows for conversion and framing overhead. The file worker has a
128 MiB hard ceiling above its initial allocator baseline. The parent bounds
frames before allocating their payloads. Each writer counts encoded bytes,
including JSON escaping, while serializing. The limits count allocator-backed
live bytes, not resident set size.

The parent discards guest `print` output and raw worker stderr. It returns
emits only after it receives success and reaps the Monty worker. Cancellation
or a deadline starts cleanup. The coordinator holds admission until it has
reaped both children. Callback trace entries contain only the tool name,
outcome, and elapsed time. They are not tool results or transcript messages.
The final second of the call budget is reserved for cleanup. A queued call
that reaches the work cutoff does not start a child. An active call cancels
its callback and reaps the guest concurrently. Both waits share the same
absolute deadline. If cleanup cannot be confirmed, the call returns a bounded
error and admission closes for the rest of the process.

Run the worker and broker tests:

```sh
cargo test --locked -p cupel-tui --features codemode-preview --test codemode_worker --test codemode_broker
```

The broker test checks cancellation during a blocked read, search walk, and
context read. It checks both child PIDs before the next script can use the
broker. It also checks the instruction snapshot, the observation handoff to
direct `apply_patch`, and result privacy. The worker tests check that a
250 ms callback shutdown fits inside the call budget and that a queued v1
call expires without spawning while two independent coordinators hold the
process slots. These tests do not qualify a release build.
