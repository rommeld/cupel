import argparse
import errno
import json
import os
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time
from datetime import UTC, datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "cupel-acp/examples"))
from direct_tool_baseline import BASELINE, PROMPT, check_results, declarations, next_result, reply, seed, tool_reply
from stdio_smoke import complete, provider, run as run_acp


PRIVATE = "PRIVATE_FIXTURE_MARKER"
SEARCH = """files = grep(pattern='Widget', outputMode='files')['files']
page = read(path=files[0]['path'], limit=3)
emit({'path': page['path'], 'line': page['text'].splitlines()[1], 'observation': page['observation']})"""
FAIL = "emit(read('src/widget.rs')['text'])\nraise ValueError('fixture')"
OVERFLOW = "emit(read('src/widget.rs')['text'])\nemit('x' * 17000)"
PATCH = "*** Begin Patch\n*** Update File: pages.txt\n@@\n-page line 1\n+patched line\n*** End Patch"


def environment(home):
    return {"PATH": str(home / "no-programs"), "HOME": str(home), "CUPEL_HOME": str(home),
            "OLLAMA_HOST": "http://127.0.0.1:9"}


def invoke(binary, home, project, response, *flags):
    with provider(home, response) as requests:
        result = subprocess.run([str(binary), "--plain", "--model", "fixture", *flags],
                                cwd=project, env=environment(home), input=PROMPT, text=True,
                                capture_output=True, timeout=20, check=True)
        return requests, result.stdout, result.stderr


def check_declarations(request, project, dates, enabled):
    expected = json.loads(BASELINE.read_text(encoding="utf-8"))
    actual = declarations(request, project, dates)
    if enabled:
        tools = actual["tools"]
        assert tools[:-1] == expected["tools"], "the ordered direct schemas changed"
        function = tools[-1]["function"]
        assert function["name"] == "codemode", tools[-1]
        assert function["parameters"] == {"type": "object", "properties": {"code": {"type": "string"}},
                                          "required": ["code"], "additionalProperties": False}
        assert actual["prompt"] == expected["prompt"].replace("- jobs\n\nGuidelines:", "- jobs\n- codemode\n\nGuidelines:", 1)
    else:
        assert actual["tools"] == expected["tools"], "disabled schemas differ from the frozen baseline"
        assert actual["prompt"] == expected["prompt"], "disabled prompt differs from the frozen baseline"


def date():
    return datetime.now(UTC).date().isoformat()


def scripted(tools, start=0):
    def respond(requests):
        index = len(requests) - 1
        if index < len(tools):
            name, args = tools[index]
            return tool_reply(start + index, name, args(requests) if callable(args) else args)
        return complete(requests)
    return respond


def transcripts(home):
    paths = list(home.rglob("*.jsonl"))
    assert paths, "no saved transcript"
    return [json.loads(line) for path in paths for line in path.read_text(encoding="utf-8").splitlines()]


def check_privacy(requests, saved, trace, names):
    assert PRIVATE not in json.dumps(requests), "intermediate source reached the provider"
    assert PRIVATE not in json.dumps(saved), "intermediate source reached the transcript"
    assert PRIVATE not in trace, "intermediate source reached the trace"
    expected = [f"fixture_{index}" for index in range(len(names))]
    assert [m["tool_call_id"] for m in requests[-1]["messages"] if m["role"] == "tool"] == expected
    results = [m for m in saved if m.get("role") == "toolResult"]
    assert [m["toolCallId"] for m in results] == expected, results
    assert [m["toolName"] for m in results] == names, results


def preview(binary, home, project):
    before = date()
    requests, _, _ = invoke(binary, home, project, reply)
    check_declarations(requests[0], project, {before, date()}, False)
    check_results(requests, project)
    print("PASS: plain mode without the flag preserves the frozen prompt and schemas")

    for path in home.rglob("*.jsonl"):
        path.unlink()
    (home / "settings.json").write_text('{"loopKiller":{"maxRepeats":1}}', encoding="utf-8")
    calls = [
        ("codemode", {"code": SEARCH}),
        ("codemode", {"code": "emit(read('pages.txt', limit=1)['observation'])"}),
        ("apply_patch", lambda requests: {"input": PATCH, "expectedObservations": [json.loads(next_result(requests[-1], 1))[0]]}),
        ("codemode", {"code": FAIL}),
        ("codemode", {"code": OVERFLOW}),
        ("codemode", {"code": "bash(command='pwd')"}),
        ("codemode", {"code": "read('pages.txt', limit=1)\nread('pages.txt', limit=1)"}),
    ]
    before = date()
    requests, trace, _ = invoke(binary, home, project, scripted(calls), "--codemode-preview")
    check_declarations(requests[0], project, {before, date()}, True)
    assert len(requests) == len(calls) + 1
    excerpt, = json.loads(next_result(requests[1], 0))
    assert excerpt["path"] == "src/widget.rs" and excerpt["line"] == "pub struct Widget;", excerpt
    assert re.fullmatch(r"R[0-9a-f]{32}-\d+", excerpt["observation"]), excerpt
    assert (project / "pages.txt").read_text(encoding="utf-8").startswith("patched line\n")
    for index, diagnostic in enumerate(("script failed", "emit limit", "capability unavailable", "blocked by tool preflight"), 3):
        result = next_result(requests[index + 1], index)
        assert diagnostic in result and len(result) < 256, result
    assert re.search(r"\[grep ok \(\d+ ms\)\]", trace), trace
    assert re.search(r"\[read blocked \(\d+ ms\)\]", trace), trace
    check_privacy(requests, transcripts(home), trace, [name for name, _ in calls])
    print("PASS: preview preserves direct schemas, emits the selected excerpt, and hands off an observation")
    print("PASS: failed emits, capability errors, and vetoes produce one bounded result without source leaks")


def worker_pids(parent):
    table = subprocess.check_output(["/bin/ps", "-axo", "pid=,ppid=,command="], text=True)
    return {int(pid) for row in table.splitlines() for pid, ppid, command in [row.split(maxsplit=2)]
            if int(ppid) == parent and command.split()[-1] in
            ("--internal-codemode-worker", "--internal-codemode-file-worker")}


def blocked_read(binary, home, project, cancel, *flags):
    fifo = project / "waiting.txt"
    fifo.unlink(missing_ok=True)
    os.mkfifo(fifo)
    calls = [("codemode", {"code": "read('waiting.txt')"})]
    if not cancel:
        calls.append(("codemode", {"code": "emit('recovered')"}))
    with provider(home, scripted(calls)) as requests, tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        with subprocess.Popen([str(binary), "--plain", *flags, "--model", "fixture"],
                              cwd=project, env=environment(home), stdin=subprocess.PIPE,
                              stdout=stdout, stderr=stderr) as process:
            writer = None
            try:
                process.stdin.write(b"test blocking read\n")
                process.stdin.close()
                until = time.monotonic() + 3
                while writer is None and time.monotonic() < until:
                    try:
                        writer = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno != errno.ENXIO:
                            raise
                        time.sleep(0.01)
                assert writer is not None, "worker never opened the fixture FIFO"
                pids = worker_pids(process.pid)
                assert len(pids) == 2, pids
                if cancel:
                    process.send_signal(signal.SIGINT)
                status = process.wait(timeout=10)
                stdout.seek(0)
                trace = stdout.read().decode()
                stderr.seek(0)
                assert status == (130 if cancel else 0), (trace, stderr.read().decode())
                for pid in pids:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        continue
                    raise AssertionError(f"worker {pid} survived the outer result")
                if cancel:
                    assert len(requests) == 1, "cancellation must not schedule another provider request"
                    assert "error: codemode cancelled" in trace, trace
                else:
                    assert len(requests) == 3, "the follow-up script did not run in the same process"
                    assert "work cutoff" in next_result(requests[1], 0)
                    assert json.loads(next_result(requests[2], 1)) == ["recovered"]
            finally:
                if writer is not None:
                    os.close(writer)
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
    print(f"PASS: {'SIGINT' if cancel else 'deadline'} ends live file work and reaps both workers")


def home_opt_in(binary, home, project, acp=False):
    def run(response, resume=None):
        if acp:
            requests, trace, warnings = run_acp(binary, home, project, reply=response, prompt=PROMPT,
                                                path=environment(home)["PATH"], resume=resume)
            return requests, json.dumps(trace), warnings
        return invoke(binary, home, project, response, *(("--resume",) if resume else ()))

    layer = home / "settings.json"
    project_layer = project / ".cupel/settings.json"
    project_layer.parent.mkdir(exist_ok=True)
    project_layer.write_text('{"codemode":{"enabled":true}}', encoding="utf-8")
    label = "ACP" if acp else "plain"
    cases = [(None, False), (False, False), ("bad", True)]
    for initial, warn in cases:
        if initial is not None:
            layer.write_text(json.dumps({"codemode": {"enabled": initial}}), encoding="utf-8")
        before = date()
        requests, _, warnings = run(complete)
        assert len(requests) == 1
        check_declarations(requests[0], project, {before, date()}, False)
        assert ("ignoring codemode" in warnings) == warn, warnings
    print(f"PASS: {label} ignores project opt-in; false and invalid home values preserve direct declarations")

    for path in home.rglob("*.jsonl"):
        path.unlink()
    layer.write_text('{"codemode":{"enabled":true}}', encoding="utf-8")
    project_layer.write_text('{"codemode":{"enabled":false}}', encoding="utf-8")
    observe = {"code": "emit(read('pages.txt', limit=1)['observation'])"}
    calls = [("codemode", {"code": SEARCH}), ("codemode", {"code": FAIL}),
             ("codemode", {"code": OVERFLOW}), ("codemode", observe)]
    before = date()
    requests, trace, _ = run(scripted(calls))
    check_declarations(requests[0], project, {before, date()}, True)
    assert len(requests) == len(calls) + 1
    selection, = json.loads(next_result(requests[1], 0))
    assert selection["path"] == "src/widget.rs" and selection["line"] == "pub struct Widget;", selection
    for index, diagnostic in [(1, "script failed"), (2, "emit limit")]:
        result = next_result(requests[index + 1], index)
        assert diagnostic in result and len(result) < 256, result
    old, = json.loads(next_result(requests[4], 3))
    assert re.search(r"grep ok \(\d+ ms\)", trace), trace
    check_privacy(requests, transcripts(home), trace, ["codemode"] * 4)
    print(f"PASS: {label} home opt-in gives one outer result per script without source leaks")

    transcript, = home.rglob("*.jsonl")
    content = (project / "pages.txt").read_bytes()
    requests, _, _ = run(scripted([("apply_patch", {"input": PATCH, "expectedObservations": [old]})], start=4), transcript.stem)
    assert len(requests) == 2
    assert "unavailable or expired" in next_result(requests[1], 4)
    assert (project / "pages.txt").read_bytes() == content
    calls = [("codemode", observe), ("apply_patch", lambda reqs: {
        "input": PATCH, "expectedObservations": [json.loads(next_result(reqs[-1], 5))[0]]})]
    requests, _, _ = run(scripted(calls, start=5), transcript.stem)
    assert len(requests) == 3
    new, = json.loads(next_result(requests[1], 5))
    assert new.rsplit("-", 1)[0] != old.rsplit("-", 1)[0], (new, old)
    assert (project / "pages.txt").read_text(encoding="utf-8").startswith("patched line\n")
    print(f"PASS: {label} resume rejects old observations and accepts a fresh script observation")


def fixture(root):
    home, project = root / "home", root / "project"
    home.mkdir(parents=True)
    project.mkdir()
    seed(project)
    return home, project


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--acp", type=Path)
    args = parser.parse_args()
    if not __debug__:
        parser.error("run without Python optimization")
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="cupel-preview-") as directory:
        root = Path(directory).resolve()
        home, project = fixture(root / "preview")
        if platform.system() == "Darwin" and platform.machine() == "arm64":
            preview(binary, home, project)
            flags = ("--codemode-preview",)
        else:
            (home / "settings.json").write_text('{"codemode":{"enabled":true}}', encoding="utf-8")
            flags = ()
        blocked_read(binary, home, project, False, *flags)
        blocked_read(binary, home, project, True, *flags)
        home_opt_in(binary, *fixture(root / "plain-home"))
        if args.acp:
            home_opt_in(args.acp.resolve(), *fixture(root / "acp-home"), acp=True)


if __name__ == "__main__":
    main()
