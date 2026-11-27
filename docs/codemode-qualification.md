# Reproduce codemode qualification

Use the `test/codemode-qualification` draft PR to test a candidate without
publishing a release. The CI job builds both binaries with Monty for macOS
universal, Linux x86-64 musl, and Linux arm64 musl. The job compares their
sizes against pre-Monty commit `824a52536ac8a6883394d965ee7180790429e751`.
It installs the release archive and runs the offline provider checks against
the installed executables with no Python on their `PATH`.

To repeat the macOS build and packaged checks, check out the pre-Monty
revision in a separate worktree. Run these commands in the candidate worktree:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash packaging/build-release.sh ../cupel-pre-monty cupel-macos-universal aarch64-apple-darwin x86_64-apple-darwin
```

The build writes the archive, checksums, and size measurements to
`qualification/`. A native smoke run of the universal archive exercises the
host slice. `lipo` checks that both slices are present.

## Run the paired evaluation

Run the evaluation on macOS with Python 3.12 or later and an OpenRouter key
that permits Qwen3.6 Plus on Alibaba. Use the candidate's packaged `cupel`.
The runner reads the key from `OPENROUTER_API_KEY` or from home settings.
It passes no key to Cupel. Its loopback proxy pins the provider route and
rejects proposed tool calls outside the read and search protocol. Both arms
receive the direct tool declarations.

```sh
python3 packaging/codemode-eval.py qualification/candidate/cupel qualification/live --revision <candidate-sha>
python3 packaging/score-codemode.py qualification/live blind
python3 packaging/score-codemode.py qualification/live compare > qualification/live/comparison.json
```

The runner reads the public fixture at commit `9814668eed43c3c941fabee21ae9d1355cf0ab53`.
It runs five pairs per prompt, alternates pair order, and uses a fresh home
for each session. Both arms use Qwen3.6 Plus, low reasoning effort, at most
4,096 output tokens per request, 12 provider requests per session, and a
240-second session limit. The model version in the run row is the OpenRouter
catalog version. The proxy checks the reported model ID and provider on each
response. The provider does not report a separate build revision in its stream.

`runs.jsonl` holds one row per session. For input usage, the runner sums
OpenRouter's `prompt_tokens` across requests. This already includes cached
input. It records `cached_tokens` separately without adding it a second time.
Missing usage remains null. Tool-result bytes count each result's UTF-8 payload
once, regardless of how often history resends it. Failed sessions remain in
the data. An enabled session that uses direct tools stays in the enabled arm.

`blind.jsonl` contains only a randomized row ID, prompt ID, and answer. The
rule-based scorer reads this packet without arm labels or metrics. It checks
for the required facts and for line citations that overlap the relevant ranges
in the pinned tree. This is a conservative text check, not a semantic review
of every statement. `scores.jsonl` records each criterion. The compare step
joins the scores to the runs and reports per-prompt medians, aggregate input
medians for the four multi-step prompts, and leave-one-pair-out sensitivity.
The known-file control is reported separately.

## Candidate results

The [qualification run](https://github.com/rommeld/cupel/actions/runs/46023488860)
tested candidate `ff6cfe0408604bd77e7b247d91b0ed51132f73e2`. The table gives
uncompressed executable sizes in bytes. The macOS figures are from the local
locked build. The Linux figures are from CI.

| Target | Executable | Pre-Monty | With Monty | Delta |
| --- | --- | ---: | ---: | ---: |
| macOS arm64 slice | `cupel` | 17,713,888 | 27,522,624 | +55.37% |
| macOS arm64 slice | `cupel-acp` | 17,343,632 | 27,136,144 | +56.46% |
| macOS x86-64 slice | `cupel` | 19,223,040 | 29,827,552 | +55.17% |
| macOS x86-64 slice | `cupel-acp` | 18,818,968 | 29,407,720 | +56.27% |
| macOS universal | `cupel` | 36,948,704 | 57,357,888 | +55.24% |
| macOS universal | `cupel-acp` | 36,168,848 | 56,561,808 | +56.38% |
| Linux x86-64 musl | `cupel` | 21,125,640 | 32,520,776 | +53.94% |
| Linux x86-64 musl | `cupel-acp` | 20,750,600 | 32,121,192 | +54.80% |
| Linux arm64 musl | `cupel` | 17,340,344 | 27,371,512 | +57.85% |
| Linux arm64 musl | `cupel-acp` | 16,948,920 | 26,980,112 | +59.18% |

The local macOS universal archive passed clean installation and the offline
provider checks with Python absent from the binaries' `PATH`. Both thin
Intel binaries also passed the home opt-in provider checks under Rosetta.

The 25 live pairs **did not pass the rollout rule**. Qwen selected direct
tools in all 25 enabled sessions. Lower medians in that arm do not demonstrate
a saving from codemode. Three guide-cache sessions reached the 12-request
cap: one off and two on. The review kept those runs in their arms.

| Prompt | Median requests off / on | Median input tokens off / on |
| --- | ---: | ---: |
| Grep rank | 3 / 3 | 18,002 / 18,173 |
| Guide cache | 10 / 11 | 160,236 / 189,438 |
| Source handoff | 5 / 3 | 47,553 / 30,118 |
| Loop killer | 4 / 3 | 46,919 / 27,547 |
| Known-file control | 2 / 2 | 7,835 / 8,197 |
| All multi-step sessions | 4 / 3.5 | 47,123 / 27,548 |

On the multi-step prompts, the blind rubric gave both arms 10/20 required-fact
passes. Citation passes fell from 3/20 off to 2/20 on. The source handoff prompt
accounted for the largest citation loss. No single pair changed the failure
decision. The known-file control had 5/5 required-fact passes in either arm,
but neither arm cited the correct lines in `tools.rs`.

See the [per-session metrics](codemode-qualification/runs.jsonl),
[blind scores](codemode-qualification/scores.jsonl), and
[comparison](codemode-qualification/comparison.json).
