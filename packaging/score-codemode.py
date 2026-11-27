import argparse
import json
import re
import statistics
import subprocess
from functools import lru_cache
from pathlib import Path


TREE = "9814668eed43c3c941fabee21ae9d1355cf0ab53"
AGENT = "crates/cupel-coding-agent/"
TOOLS = AGENT + "src/tools/"
FACTS = {
    "rank": {
        "definition_before_callers": r"defines.{0,38}(searched|target|name)|definitions? (first|of the exact|outrank)|defines_name",
        "definition_shaped_tier": r"definition[- _]shaped",
        "path_buckets": r"source.{0,140}test.{0,140}(low.priority|peripher|examples)",
        "capped_count": r"(?:cap|clamp).{0,140}(10|DENSITY_CAP)|(?:min\(10|DENSITY_CAP).{0,140}(count|match|density)",
        "stable_path_ties": r"(ties?|equal).{0,150}(path order|lexicograph|alphabetic|BTreeMap)|stable.{0,120}(path|BTreeMap)",
    },
    "cache": {
        "before_is_snapshot": r"(?:before|during).{0,180}(?:snapshot|cached|old|original)|(?:snapshot|cached|original).{0,180}(?:unchanged|on disk|before)",
        "after_reads_new_snapshot": r"reload.{0,210}(?:new|fresh|updated|rebuild|re[- ]?read).{0,100}(?:guide|cache|snapshot|content)|(?:fresh|new|updated).{0,150}(?:guide|cache|snapshot).{0,120}reload",
        "loads_linked_guides": r"load_guides|(?:links|linked guides).{0,120}(?:load|cache|snapshot)",
        "reader_binding": r"bind_session.{0,250}(?:with_instructions|ReadTool|cache)|(?:with_instructions|ReadTool).{0,250}bind_session",
        "cache_before_live_disk": r"(?:cache|snapshot).{0,220}(?:fallback|fall back|read_to_string|instead of (?:live|disk)|rather than (?:live|disk))",
    },
    "patch": {
        "check_requires_an_observation": r"expected_?observations?|(?:if|with|suppl\w+) .{0,60}observation (?:id|precondition)",
        "whole_file_revision": r"(?:whole|entire|full|raw)[- ](?:file|bytes)|(?:entire|whole) (?:raw )?(?:content|file)",
        "neither_file_changes": r"neither file|both files.{0,40}(?:unchanged|not modified)|no (?:file|disk) writes|before any (?:file is written|writes)",
        "regression_test": r"whole_file_preconditions_reject_unrelated_edits_before_any_writes",
    },
    "loop": {
        "project_wins": r"project (?:settings? )?(?:wins|overrides|takes precedence|has priority)|project\.loop_killer\.or\(home\.loop_killer\)",
        "zero_disables": r"(?:zero|\b0\b).{0,115}(?:disabl|turn\w* (?:it |the loop killer )?off|None)|disabl.{0,70}\b0\b",
        "layering": r"(?:Settings::)?layered\b",
        "normalization": r"loop_killer_max_repeats",
        "hook_enforcement": r"before_tool_call.{0,250}(?:note_call|block|veto)|(?:note_call|block|veto).{0,250}before_tool_call",
    },
    "known": {
        "tool": r"apply_patch",
        "per_file_queue": r"file_queue",
        "parallel_batches": r"(?:parallel|concurrent).{0,65}(?:tool|batch|calls)|(?:tool|batch|calls).{0,65}(?:parallel|concurrent)",
    },
}
EVIDENCE = {
    "rank": {TOOLS + "grep_rank.rs": [(11, 20), (33, 44), (58, 65), (109, 122)],
             TOOLS + "grep.rs": [(172, 187), (209, 228)]},
    "cache": {AGENT + "src/instruction_cache.rs": [(30, 56), (83, 99)],
              AGENT + "src/bootstrap.rs": [(73, 93), (112, 116)],
              TOOLS + "read.rs": [(246, 285)], "crates/cupel-tui/src/app.rs": [(984, 995)]},
    "patch": {TOOLS + "apply_patch.rs": [(247, 259), (375, 397), (69, 80)],
              AGENT + "tests/source_handoffs.rs": [(66, 97)]},
    "loop": {AGENT + "src/settings.rs": [(136, 143), (148, 169)],
             AGENT + "src/bootstrap.rs": [(234, 252)]},
    "known": {AGENT + "src/tools.rs": [(12, 13)]},
}
PATH_LINE = re.compile(r"([\w./-]+\.rs)(?:[`*\])\s,]|\]\([^\n)]*\))*"
                       r"(?:#L?|:L?|\blines?\s+)(\d+)(?:\s*(?:[-–—]L?|to\s+)(\d+))?", re.I)
LINE_PATH = re.compile(r"\blines?\s+(\d+)(?:\s*[-–—]\s*(\d+))?"
                       r"[\s*`]+of\s+[`*]*([\w./-]+\.rs)", re.I)


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines()]


@lru_cache
def file_lines(path):
    return len(subprocess.check_output(["git", "show", TREE + ":" + path]).splitlines())


def score(row):
    answer, prompt = row["answer"], row["prompt"]
    text = re.sub(r"\s+", " ", answer.replace("`", "").replace("*", ""))
    facts = {name: bool(re.search(pattern, text, re.I)) for name, pattern in FACTS[prompt].items()}
    cited = PATH_LINE.findall(answer) + [(path, start, end) for start, end, path in LINE_PATH.findall(answer)]
    evidence = {path: False for path in EVIDENCE[prompt]}
    for cited_path, start, end in cited:
        start, end = int(start), int(end or start)
        for path, windows in EVIDENCE[prompt].items():
            if not (cited_path == path or path.endswith("/" + cited_path)):
                continue
            if 1 <= start <= end <= file_lines(path) and end - start <= 65:
                evidence[path] |= any(start <= hi and end >= lo for lo, hi in windows)
    return dict(id=row["id"], facts=facts, evidence=evidence,
                required_facts_pass=all(facts.values()), citations_pass=all(evidence.values()))


def comparison(runs):
    metrics = ("provider_requests", "input_tokens", "tool_result_bytes", "wall_seconds", "callback_count")
    report = {}
    for prompt in (*FACTS, "multi_step"):
        summary = {}
        for enabled in (False, True):
            group = [row for row in runs if row["enabled"] == enabled and
                     (row["prompt"] == prompt or (prompt == "multi_step" and row["prompt"] != "known"))]
            medians = {metric: statistics.median(row[metric] for row in group)
                       if group and all(row[metric] is not None for row in group) else None for metric in metrics}
            summary["on" if enabled else "off"] = dict(sessions=len(group), medians=medians,
                required_facts_pass=sum(row["required_facts_pass"] for row in group),
                citations_pass=sum(row["citations_pass"] for row in group),
                codemode_used=sum(row["codemode_used"] for row in group),
                failed_sessions=sum(bool(row["errors"]) for row in group))
        report[prompt] = summary
    return report


def gate(report):
    multi = report["multi_step"]
    on, off = multi["on"], multi["off"]
    on_tokens, off_tokens = on["medians"]["input_tokens"], off["medians"]["input_tokens"]
    return (on["failed_sessions"] + off["failed_sessions"] == 0
            and all(on[metric] >= off[metric] for metric in ("required_facts_pass", "citations_pass"))
            and sum(report[prompt]["off"]["medians"]["provider_requests"] -
                    report[prompt]["on"]["medians"]["provider_requests"] >= 1
                    for prompt in FACTS if prompt != "known") >= 2
            and on_tokens is not None and off_tokens is not None and on_tokens < off_tokens)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("operation", choices=("blind", "compare"))
    args = parser.parse_args()
    score_path = args.directory / "scores.jsonl"
    if args.operation == "blind":
        with score_path.open("x") as output:
            for row in rows(args.directory / "blind.jsonl"):
                output.write(json.dumps(score(row)) + "\n")
        return
    scores = {row["id"]: row for row in rows(score_path)}
    runs = [row | scores.pop(row["id"]) for row in rows(args.directory / "runs.jsonl")]
    assert not scores and len(runs) == 50, "expected scores for 50 sessions"
    pairs = {(row["prompt"], row["pair"]) for row in runs}
    assert len(pairs) == 25 and all({row["enabled"] for row in runs if (row["prompt"], row["pair"]) == pair}
                                    == {False, True} for pair in pairs), "expected 25 matched pairs"
    report = comparison(runs)
    passed = gate(report)
    sensitive = [pair for pair in sorted(pairs) if gate(comparison([
        row for row in runs if (row["prompt"], row["pair"]) != pair])) != passed]
    print(json.dumps(dict(summary=report, live_gate_pass=passed and not sensitive,
                          decision_changes_without=sensitive), indent=2))


if __name__ == "__main__":
    main()
