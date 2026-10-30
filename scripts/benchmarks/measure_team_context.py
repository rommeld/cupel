#!/usr/bin/env python3
"""Compare fixed team-context captures; optionally meter them with billable Fireworks calls.

Requires tiktoken only for local token counts. This script does not install dependencies.
The default mode never reads credentials or makes inference calls.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import urllib.error
import urllib.request


def content_tokens(request, encoding):
    return len(encoding.encode(request["system"])) + sum(
        len(encoding.encode(message["content"])) for message in request["messages"]
    )


def replay(request, key, model):
    messages = [{"role": "system", "content": request["system"]}, *request["messages"]]
    messages.append({
        "role": "user",
        "content": "For this token-usage replay, reply with exactly OK. Do not call tools.",
    })
    body = json.dumps({
        "model": model, "messages": messages, "temperature": 0,
        "max_tokens": 4096, "reasoning_effort": "low",
    }).encode()
    call = urllib.request.Request(
        "https://api.fireworks.ai/inference/v1/chat/completions",
        data=body,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(call, timeout=90) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"Fireworks replay failed with HTTP {error.code}; no retry made") from None
    choice = result["choices"][0]
    # Prompt usage is measurable independently of a stochastic output. Do not
    # discard expensive/nonconforming responses and bias the comparison.
    return {
        **result["usage"],
        "completed": choice["finish_reason"] == "stop",
        "response_matches": choice["message"].get("content", "").strip() == "OK",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--fireworks", action="store_true", help="make billable paired inference calls")
    parser.add_argument("--model", default="accounts/fireworks/models/minimax-m3p1-flash")
    args = parser.parse_args()
    import tiktoken  # Optional analysis dependency, not a Cupel runtime dependency.

    encoding = tiktoken.get_encoding("o200k_base")
    before = json.loads(args.before.read_text())
    after = json.loads(args.after.read_text())
    if [s["updates"] for s in before] != [s["updates"] for s in after]:
        raise ValueError("captures describe different workloads")
    key = None
    if args.fireworks:
        key = os.environ.get("FIREWORKS_API_KEY")
        if not key:
            home = Path(os.environ.get("CUPEL_HOME", str(Path.home() / ".cupel")))
            key = json.loads((home / "settings.json").read_text()).get("providers", {}).get("fireworks")
        if not key:
            raise ValueError("Fireworks credential required for explicit billable replay")
    report = {
        "tokenizer": "o200k_base",
        "scope": "system and message text only; excludes role framing, tools and output",
        "captures_sha256": {
            side: hashlib.sha256(path.read_bytes()).hexdigest()
            for side, path in [("before", args.before), ("after", args.after)]
        },
        "scenarios": [],
    }
    for old, new in zip(before, after, strict=True):
        if len(old["requests"]) != len(new["requests"]):
            raise ValueError("request counts differ; cannot make a matched comparison")
        row = {"updates": old["updates"], "requests_per_version": len(old["requests"])}
        for side, scenario in [("before", old), ("after", new)]:
            row[side] = {
                "content_tokens": sum(content_tokens(r, encoding) for r in scenario["requests"]),
                "requests_with_evidence": sum(
                    "src/parser.rs:42" in "\n".join(m["content"] for m in r["messages"])
                    for r in scenario["requests"]
                ),
            }
        row["content_reduction_percent"] = round(
            100 * (1 - row["after"]["content_tokens"] / row["before"]["content_tokens"]), 2
        )
        if key:
            row["metered"] = {"model": args.model, "before": [], "after": []}
            for i, (old_request, new_request) in enumerate(zip(old["requests"], new["requests"], strict=True)):
                # Alternate order to reduce systematic cache/order bias. Compare total
                # prompt tokens, including cache hits, not just uncached/billed input.
                pairs = [("before", old_request), ("after", new_request)]
                if i % 2:
                    pairs.reverse()
                for side, request in pairs:
                    row["metered"][side].append(replay(request, key, args.model))
            for side in ("before", "after"):
                row[side]["metered_prompt_tokens"] = sum(u["prompt_tokens"] for u in row["metered"][side])
                row[side]["metered_output_tokens"] = sum(u["completion_tokens"] for u in row["metered"][side])
        report["scenarios"].append(row)
    rendered = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.write_text(rendered)
    print(rendered, end="")


if __name__ == "__main__":
    main()
