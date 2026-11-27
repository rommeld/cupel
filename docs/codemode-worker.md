# Codemode preview worker

The `codemode-preview` Cargo feature builds the private Monty worker into
`cupel`. It does not register a tool or add a public flag. The worker uses the
same executable, a cleared environment, and `/` as its working directory.
Monty receives no filesystem mounts. The worker rejects all Monty OS calls and
forwards only `read` and `grep` requests. It does not execute those requests.

| Limit | Preview budget |
| --- | --- |
| Active scripts | 1 per process-owned coordinator |
| Script source | 16 KiB of UTF-8 |
| Guest memory | 16 MiB above Monty's allocator baseline |
| Hard allocator ceiling | 20 MiB above the worker's initial allocator baseline |
| Host calls | 16 sequential `read` or `grep` calls |
| Callback arguments | 16 KiB of JSON per request |
| Callback results | 256 KiB cumulative JSON |
| Emits | 16 KiB cumulative JSON |
| Guest execution | 1 second, excluding worker startup and time awaiting host callbacks |
| Call deadline | 5 seconds, including admission, worker startup, and host callbacks |

Only the worker arms the hard allocator limit. The 4 MiB of headroom allows
for conversion and framing overhead before the hard ceiling stops the process.
The memory limits count allocator-backed live bytes, not resident set size.
The parent bounds frames before allocating their payloads. Both processes
count encoded bytes, including JSON escaping, as they write buffers.

The parent discards guest `print` output and raw worker stderr. It returns
emits only after it receives success and reaps the worker. Cancellation or
deadline starts cleanup. The coordinator does not release admission until
the worker has exited and any active callback reports that it has stopped.
If cleanup cannot be confirmed, the coordinator closes admission for the
rest of the process. Cleanup can take up to one second per wait beyond the
call deadline.

Run the worker tests with `cargo test --locked -p cupel-tui --features
codemode-preview --test codemode_worker`. These tests start the native `cupel`
worker with no Python on `PATH` and use fixture callbacks. Real filesystem
cancellation and the provider and transcript checks belong to the preview
integration. The worker tests do not qualify a release build.
