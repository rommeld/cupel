#!/usr/bin/env python3
"""Run prebuilt live_agents test binaries on matched, disposable Fireworks fixtures.

Explicit --live opt-in is required. Each workflow makes billable model calls.
The Rust fixture records requests/responses locally with credentials redacted.
Failed workflows stay in the report; comparisons never select only passing runs.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


def summarize(trace):
    rows = [json.loads(line) for line in trace.read_text().splitlines()]
    requests = {row["id"]: row for row in rows if row["event"] == "request"}
    responses = {row["id"]: row for row in rows if row["event"] == "response"}
    agents = defaultdict(Counter)
    tools = Counter()
    reads = Counter()
    observed_errors = Counter()
    observed_results = set()
    profiles = []
    for request_id, request in requests.items():
        agent = request.get("session") or "lead"
        for message in request["context"]["messages"]:
            if message.get("role") == "toolResult":
                key = (agent, message["toolCallId"])
                if key not in observed_results:
                    observed_results.add(key)
                    if message.get("isError"):
                        observed_errors[message["toolName"]] += 1
        agents[agent]["requests"] += 1
        response = responses.get(request_id)
        if response is None:
            agents[agent]["missing_responses"] += 1
            continue
        message = response["message"]
        usage = message["usage"]
        if message.get("stopReason") in ("error", "aborted") and usage.get("totalTokens", 0) == 0:
            agents[agent]["unmetered_responses"] += 1
        metrics = {
            "input": usage["input"] + usage["cacheRead"] + usage["cacheWrite"],
            "uncached_input": usage["input"],
            "cached_input": usage["cacheRead"],
            "cache_write": usage["cacheWrite"],
            "output": usage["output"],
            "reasoning": usage.get("reasoning", 0),
            "cost_usd": usage["cost"]["total"],
        }
        metrics["total"] = metrics["input"] + metrics["output"]
        agents[agent].update(metrics)
        calls = [c["name"] for c in message["content"] if c["type"] == "toolCall"]
        for call in message["content"]:
            if call["type"] == "toolCall" and call["name"] == "read":
                reads[f"{agent}:{call['arguments'].get('path', '?')}"] += 1
        tools.update(calls)
        profiles.append({"id": request_id, "agent": agent, **metrics, "tools": calls})
    totals = Counter()
    for values in agents.values():
        totals.update(values)
    return {
        "totals": dict(totals), "agents": {k: dict(v) for k, v in agents.items()},
        "tools": dict(tools), "requests": profiles,
        "file_reads": dict(reads),
        "observed_tool_errors": dict(observed_errors),
        "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
        "apis": sorted({request["model"]["api"] for request in requests.values()}),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", action="append", required=True, metavar="NAME=BINARY")
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, default=Path("/tmp/cupel-matched-fixture"))
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--reuse", action="store_true", help="include already-recorded runs in this directory")
    parser.add_argument("--workflow", choices=("matched", "full"), default="matched")
    args = parser.parse_args()
    if not args.live:
        parser.error("--live is required: this benchmark makes billable Fireworks calls")
    variants = dict(value.split("=", 1) for value in args.variant)
    args.output.mkdir(parents=True, exist_ok=True)
    report = {
        "method": "alternating order; identical fixture path, model/effort/output ceilings; temperature 0 requested (Anthropic extended thinking omits temperature); no seed; no response filtering",
        "fixture": str(args.fixture), "runs": [],
        "workflow": args.workflow,
        "binaries": {name: hashlib.sha256(Path(path).read_bytes()).hexdigest() for name, path in variants.items()},
    }
    for round_id in range(args.rounds):
        order = list(variants)
        if round_id % 2:
            order.reverse()
        for name in order:
            stem = args.output / f"{name}-{round_id}"
            trace = stem.with_suffix(".jsonl")
            log = stem.with_suffix(".log")
            reused = args.reuse and trace.exists() and log.exists()
            if not reused:
                if trace.exists() or log.exists() or args.fixture.exists():
                    raise RuntimeError("refusing to overwrite a trace/log or reuse a live fixture directory")
                env = dict(os.environ, CUPEL_LIVE_AGENTS="1", CUPEL_LIVE_AGENTS_TRACE=str(trace), CUPEL_LIVE_AGENTS_ROOT=str(args.fixture))
                env.pop("CUPEL_LIVE_EXTENDED_ONLY", None)
                env.pop("CUPEL_LIVE_MATCHED_TASK", None)
                if args.workflow == "matched":
                    env["CUPEL_LIVE_MATCHED_TASK"] = "1"
                print(f"Running {name} round {round_id}", flush=True)
                with log.open("x") as output:
                    try:
                        result = subprocess.run([variants[name], "--ignored", "--nocapture", "fireworks_multi_agent_baseline"], env=env, stdout=output, stderr=subprocess.STDOUT, timeout=360)
                        passed = result.returncode == 0
                    except subprocess.TimeoutExpired:
                        output.write("\nBENCHMARK TIMEOUT: failed workflow retained\n")
                        passed = False
            else:
                passed = "test result: ok. 1 passed; 0 failed" in log.read_text()
            row = {"variant": name, "round": round_id, "passed": passed, "reused": reused, **summarize(trace)}
            log_text = log.read_text()
            row["checks"] = [line for line in log_text.splitlines() if line.startswith("PASS ")]
            combined = re.search(r"(?:Matched|Fireworks) combined: (\d+) input, (\d+) output, (\d+) cached, \$([\d.]+)", log_text)
            if combined:
                uncached, output, cached = map(int, combined.group(1, 2, 3))
                row["accounting_reconciled"] = (
                    row["totals"].get("uncached_input", 0) == uncached
                    and row["totals"].get("output", 0) == output
                    and row["totals"].get("cached_input", 0) == cached
                    and abs(row["totals"].get("cost_usd", 0) - float(combined.group(4))) < 0.000001
                )
                if not row["accounting_reconciled"]:
                    raise RuntimeError("incomplete provider recording: trace does not reconcile with application totals")
            if passed and not row.get("accounting_reconciled"):
                raise RuntimeError("passing run must reconcile all provider usage")
            report["runs"].append(row)
            (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
            print(name, round_id, "PASS" if passed else "FAIL", row["totals"], flush=True)


if __name__ == "__main__":
    main()
