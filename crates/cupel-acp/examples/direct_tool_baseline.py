import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
from datetime import UTC, datetime
from pathlib import Path

from stdio_smoke import complete, run


BASELINE = Path(__file__).with_name("direct_tool_baseline.json")
PROMPT = "Find the Widget definition and callers, then read the fixture pages."
CALLS = (
    ("grep", {"pattern": "Widget", "outputMode": "files"}),
    ("grep", {"pattern": "Widget", "outputMode": "files", "limit": 2}),
    ("grep", {"pattern": "Widget", "context": 1}),
    ("grep", {"pattern": "Widget", "path": "src", "limit": 1}),
    ("grep", {"pattern": "long-hit", "context": 200, "limit": 1}),
    ("read", {"path": "pages.txt"}),
    ("read", {"path": "pages.txt", "offset": 2001, "limit": 1}),
)
RANKED = (
    "src/widget.rs:2 [def] (1 match)  pub struct Widget;",
    "src/caller.rs:1 (2 matches)  use crate::Widget;",
    "tests/widget.rs:1 (1 match, test)  use app::Widget;",
)
LONG_TEXT = "long-hit\n" + ("x" * 600 + "\n") * 200
PAGES = tuple(f"page line {n}" for n in range(1, 2003))


def seed(project):
    subprocess.run(
        ["git", "init", "--quiet", "--template="], cwd=project, check=True,
        env={"PATH": os.environ["PATH"], "HOME": str(project.parent / "home"),
             "GIT_CONFIG_NOSYSTEM": "1"},
    )
    files = {
        ".gitignore": "ignored.rs\n",
        "ignored.rs": "pub struct Widget; // ignored fixture\n",
        "src/caller.rs": "use crate::Widget;\nfn go() {\n    Widget::new();\n}\n",
        "src/widget.rs": "// definition file\npub struct Widget;\nPRIVATE_FIXTURE_MARKER\n",
        "tests/widget.rs": "use app::Widget;\n",
        "pages.txt": "\n".join(PAGES) + "\n",
        "long.txt": LONG_TEXT,
    }
    for name, text in files.items():
        path = project / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")


def tool_reply(index, name, args):
    return {"tool_calls": [{
        "index": 0, "id": f"fixture_{index}", "type": "function",
        "function": {"name": name, "arguments": json.dumps(args)},
    }]}, "tool_calls"


def reply(requests):
    index = len(requests) - 1
    if index < len(CALLS):
        return tool_reply(index, *CALLS[index])
    return complete(requests)


def declarations(request, project, allowed_dates):
    system = [message for message in request["messages"] if message["role"] == "system"]
    assert len(system) == 1, system
    prompt = system[0]["content"].replace(str(project), "<PROJECT>")
    date = re.findall(r"(?m)^Current date: (\d{4}-\d{2}-\d{2})$", prompt)
    assert len(date) == 1 and date[0] in allowed_dates, date
    return {
        "prompt": prompt.replace(f"\nCurrent date: {date[0]}\n", "\nCurrent date: <UTC_DATE>\n"),
        "tools": request["tools"],
    }


def next_result(request, index):
    message = request["messages"][-1]
    assert message["role"] == "tool" and message["tool_call_id"] == f"fixture_{index}", message
    return message["content"]


def read_page(result, project, start, end, sequence, body):
    match = re.match(rf"\[Observation: (R[0-9a-f]{{32}}-{sequence})\n", result)
    assert match, result[:200]
    observation = match[1]
    revision = hashlib.sha256(("\n".join(PAGES) + "\n").encode()).hexdigest()
    header = (
        f"[Observation: {observation}\nCheckout: {project}\nPath: pages.txt\n"
        f"Revision: sha256:{revision}\nRange: {start}-{end} of 2002 lines\nCoverage: partial file]"
    )
    assert result == f"{header}\n\n{body}", (result[:300], result[-300:])
    return observation


def check_results(requests, project):
    assert len(requests) == len(CALLS) + 1, f"expected {len(CALLS) + 1} provider requests"
    results = [next_result(request, index) for index, request in enumerate(requests[1:])]
    assert results[0] == "\n".join(RANKED), results[0]
    assert results[1] == (
        "\n".join(RANKED[:2]) + "\n\n[2 of 3 files shown. Use limit=4 for more, or refine pattern]"
    ), results[1]
    print("PASS: ranked grep previews, definition priority, ignored files, and file limit")

    assert results[2] == (
        "src/caller.rs:1: use crate::Widget;\n"
        "src/caller.rs-2- fn go() {\n"
        "src/caller.rs:3:     Widget::new();\n"
        "src/caller.rs-4- }\n"
        "src/widget.rs-1- // definition file\n"
        "src/widget.rs:2: pub struct Widget;\n"
        "src/widget.rs-3- PRIVATE_FIXTURE_MARKER\n"
        "tests/widget.rs:1: use app::Widget;"
    ), results[2]
    assert results[3] == (
        "src/caller.rs:1: use crate::Widget;\n\n"
        "[1 matches limit reached. Use limit=2 for more, or refine pattern]"
    ), results[3]
    long_rows = ["long.txt:1: long-hit"]
    long_rows.extend(f"long.txt-{n}- {'x' * 500}... [truncated]" for n in range(2, 98))
    assert results[4] == (
        "\n".join(long_rows) +
        "\n\n[1 matches limit reached. Use limit=2 for more, or refine pattern. 50.0KB limit reached. "
        "Some lines truncated to 500 chars. Use read tool to see full lines]"
    ), (len(results[4].encode()), results[4][-300:])
    print("PASS: grep content order, scoped paths, context, match and truncation notices")

    first = read_page(results[5], project, 1, 2000, 1, "\n".join(PAGES[:2000]) +
                      "\n\n[Showing lines 1-2000 of 2002. Use offset=2001 to continue.]")
    second = read_page(results[6], project, 2001, 2001, 2, PAGES[2000] +
                       "\n\n\n[1 more lines in file. Use offset=2002 to continue.]")
    assert first.rsplit("-", 1)[0] == second.rsplit("-", 1)[0]
    print("PASS: read text, observation, hard line limit, and next page")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    if not __debug__:
        parser.error("the baseline check requires Python assertions")
    expected = json.loads(BASELINE.read_text(encoding="utf-8"))
    with tempfile.TemporaryDirectory(prefix="cupel-direct-tools-") as root:
        root = Path(root).resolve()
        home, project = root / "home", root / "project"
        home.mkdir()
        project.mkdir()
        seed(project)
        before = datetime.now(UTC).date().isoformat()
        requests, _, _ = run(args.binary.resolve(), home, project, reply=reply, prompt=PROMPT)
        after = datetime.now(UTC).date().isoformat()
        observed = declarations(requests[0], project, {before, after})
        assert observed["prompt"] == expected["prompt"], "system prompt differs from the baseline"
        assert observed["tools"] == expected["tools"], "tool schemas or order differ from the baseline"
        print(f"PASS: direct prompt and ordered tool schemas from {expected['source']}")
        check_results(requests, project)


if __name__ == "__main__":
    main()
