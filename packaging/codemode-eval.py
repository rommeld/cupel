import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
from pathlib import Path


TREE = "9814668eed43c3c941fabee21ae9d1355cf0ab53"
MODEL = "qwen/qwen3.6-plus"
MODEL_VERSION = "qwen/qwen3.6-plus-04-02"
ROUTE = "Alibaba"
THINKING = "low"
MAX_TOKENS, MAX_REQUESTS, TIMEOUT = 4096, 12, 240
PROMPTS = {
    "rank": "In `grep` files mode, what puts a definition above its callers, how do tests and match counts affect the order, and what breaks ties?",
    "cache": "If a project guide linked from `AGENTS.md` changes on disk during a session, what does `read` return before and after TUI hot reload? Trace how the reader gets the cache.",
    "patch": "A model reads one line, then a different line in the same file changes before it submits a two-file patch. Does either file change, and which regression test proves it?",
    "loop": "A user sets `loopKiller.maxRepeats` in both home and project settings. Which wins, what does zero do, and where is the decision enforced?",
    "known": "Open `crates/cupel-coding-agent/src/tools.rs`. Which tool serializes mutations per file, and why?",
}
INSTRUCTIONS = "Inspect with read/search only, do not use shell or change files, and cite file paths and line numbers."


def key():
    if value := os.getenv("OPENROUTER_API_KEY"):
        return value
    home = Path(os.getenv("CUPEL_HOME", str(Path.home() / ".cupel")))
    value = json.loads((home / "settings.json").read_text())["providers"]["openrouter"]
    return value if isinstance(value, str) else value["apiKey"]


class Proxy(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def respond(self, status, body, mime):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        server = self.server
        if len(server.requests) >= MAX_REQUESTS:
            server.errors.append("provider request budget")
            self.send_error(429)
            return
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 < length <= 8_000_000:
            self.send_error(413)
            return
        request = json.loads(self.rfile.read(length))
        row = {"prompt_tokens": None, "cached_tokens": None, "model": None, "provider": None}
        server.requests.append(row)
        names = [tool["function"]["name"] for tool in request.get("tools", [])]
        if not {"read", "grep", "apply_patch", "bash"}.issubset(names) or ("codemode" in names) != server.enabled:
            server.errors.append("unexpected provider declarations")
            self.send_error(422)
            return
        for message in request["messages"]:
            if message["role"] == "tool":
                content = message.get("content")
                if not isinstance(content, str):
                    content = json.dumps(content, ensure_ascii=False, separators=(",", ":"))
                server.results[message.get("tool_call_id")] = len(content.encode())
        request.update(model=MODEL, stream=True, stream_options={"include_usage": True},
                       provider={"only": [ROUTE], "allow_fallbacks": False},
                       reasoning={"effort": THINKING}, max_tokens=MAX_TOKENS)
        upstream = urllib.request.Request("https://openrouter.ai/api/v1/chat/completions",
                                          data=json.dumps(request).encode(), headers={
                                              "Content-Type": "application/json",
                                              "Authorization": "Bearer " + server.key,
                                          })
        try:
            with urllib.request.urlopen(upstream, timeout=45) as response:
                body = response.read(4_000_001)
            if len(body) > 4_000_000:
                raise ValueError("provider response size budget")
            calls = {}
            for line in body.splitlines():
                if not line.startswith(b"data:") or line[5:].strip() == b"[DONE]":
                    continue
                event = json.loads(line[5:])
                for name in ("model", "provider"):
                    if event.get(name) is not None:
                        row[name] = event[name]
                if usage := event.get("usage"):
                    row["prompt_tokens"] = usage.get("prompt_tokens")
                    row["cached_tokens"] = (usage.get("prompt_tokens_details") or {}).get("cached_tokens")
                if event.get("error"):
                    raise ValueError("provider stream error")
                for choice in event.get("choices", []):
                    for call in (choice.get("delta") or {}).get("tool_calls", []):
                        index = call.get("index", 0)
                        calls[index] = calls.get(index, "") + (call.get("function") or {}).get("name", "")
            if row["model"] != MODEL or row["provider"] != ROUTE:
                raise ValueError("provider model or route drift")
            allowed = {"read", "grep", "codemode"} if server.enabled else {"read", "grep"}
            if refused := set(calls.values()) - allowed:
                raise ValueError("tool outside the read/search protocol: " + ", ".join(sorted(refused)))
        except urllib.error.HTTPError as error:
            server.errors.append(f"upstream HTTP {error.code}")
            self.send_error(502)
        except (OSError, ValueError) as error:
            server.errors.append(str(error))
            self.send_error(502)
        else:
            self.respond(200, body, "text/event-stream")


def sandbox(binary, project, home, port):
    paths = lambda entries: " ".join(f"(subpath {json.dumps(str(path))})" for path in entries)
    return (f'(version 1)(deny default)(import "system.sb")'
            f'(allow file-read* {paths([project, home])})'
            f'(allow file-read* (literal {json.dumps(str(binary))}))(allow file-read-metadata)'
            f'(allow file-write* {paths([home, "/dev/null"])})'
            f'(allow process-exec (literal {json.dumps(str(binary))}))(allow process-fork)'
            f'(allow signal (target self))(allow network-outbound (remote tcp "localhost:{port}"))')


def transcript(home):
    messages = []
    for path in home.rglob("*.jsonl"):
        messages.extend(json.loads(line) for line in path.read_text().splitlines())
    return messages


def run(binary, project, credential, enabled, question):
    with tempfile.TemporaryDirectory(prefix="cupel-eval-home-") as directory, ThreadingHTTPServer(("127.0.0.1", 0), Proxy) as proxy:
        home = Path(directory).resolve()
        proxy.key, proxy.enabled, proxy.requests, proxy.errors, proxy.results = credential, enabled, [], [], {}
        (home / "settings.json").write_text(json.dumps({"codemode": {"enabled": enabled}, "checkForUpdates": False}))
        (home / "models.json").write_text(json.dumps([{
            "id": MODEL, "name": MODEL_VERSION, "api": "openai-completions", "provider": "openrouter",
            "baseUrl": f"http://127.0.0.1:{proxy.server_port}/v1", "reasoning": True, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0},
            "contextWindow": 131072, "maxTokens": MAX_TOKENS,
            "compat": {"requiresApiKey": False, "thinkingFormat": "openrouter"},
        }]))
        worker = threading.Thread(target=proxy.serve_forever, daemon=True)
        worker.start()
        env = {"PATH": str(home / "empty"), "HOME": str(home), "CUPEL_HOME": str(home),
               "TMPDIR": str(home), "OLLAMA_HOST": "http://127.0.0.1:9"}
        command = ["/usr/bin/sandbox-exec", "-p", sandbox(binary, project, home, proxy.server_port),
                   str(binary), "--plain", "--model", MODEL, "--thinking", THINKING]
        start = time.monotonic()
        try:
            try:
                result = subprocess.run(command, cwd=project, env=env, text=True,
                                        input=f"{INSTRUCTIONS}\n\n{question}\n", capture_output=True, timeout=TIMEOUT)
                trace = result.stdout
                if result.returncode:
                    proxy.errors.append(f"cupel exit {result.returncode}: {result.stderr.strip()[-240:]}")
            except subprocess.TimeoutExpired as error:
                trace = (error.stdout or b"").decode(errors="replace")
                proxy.errors.append("session wall-time budget")
            elapsed = time.monotonic() - start
        finally:
            proxy.shutdown()
            worker.join()
        messages = transcript(home)
        assistants = [message for message in messages if message.get("role") == "assistant"]
        final = assistants[-1] if assistants else {}
        answer = "\n".join(item["text"] for item in final.get("content", []) if item.get("type") == "text")
        if final.get("stopReason") not in {"stop", None}:
            proxy.errors.append(str(final["stopReason"]))
        if not answer:
            proxy.errors.append("missing final answer")
        usage = [request["prompt_tokens"] for request in proxy.requests]
        results = [message for message in messages if message.get("role") == "toolResult"]
        callbacks = re.findall(r"^\[(read|grep) (ok|failed|blocked|cancelled) \(\d+ ms\)\]$", trace, re.MULTILINE)
        return answer, dict(provider_requests=len(proxy.requests), request_usage=proxy.requests,
                            input_tokens=sum(usage) if usage and None not in usage else None,
                            tool_result_bytes=sum(proxy.results.values()), wall_seconds=round(elapsed, 3),
                            callback_count=len(callbacks), codemode_used=any(x.get("toolName") == "codemode" for x in results),
                            errors=proxy.errors, tool_errors=[x.get("toolName") for x in results if x.get("isError")])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--revision", required=True)
    parser.add_argument("--pairs", type=int, choices=range(1, 6), default=5)
    parser.add_argument("--prompt", action="append", choices=PROMPTS)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this runner requires the macOS sandbox")
    binary, output = args.binary.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    credential = key()
    snapshot = subprocess.check_output(["git", "archive", TREE])
    lock = threading.Lock()
    with tempfile.TemporaryDirectory(prefix="cupel-eval-tree-") as directory:
        project = Path(directory).resolve()
        with tarfile.open(fileobj=BytesIO(snapshot)) as archive:
            archive.extractall(project, filter="data")
        (project / ".git").mkdir()
        with (output / "runs.jsonl").open("x") as runs, (output / "blind.jsonl").open("x") as blind:
            def prompt_pairs(prompt):
                for pair in range(1, args.pairs + 1):
                    arms = (False, True) if (pair + list(PROMPTS).index(prompt)) % 2 else (True, False)
                    for order, enabled in enumerate(arms, 1):
                        answer, metrics = run(binary, project, credential, enabled, PROMPTS[prompt])
                        identity = uuid.uuid4().hex
                        with lock:
                            blind.write(json.dumps(dict(id=identity, prompt=prompt, answer=answer)) + "\n")
                            blind.flush()
                            row = dict(id=identity, prompt=prompt, pair=pair, enabled=enabled, order=order,
                                       tree=TREE, candidate=args.revision, binary_sha256=digest,
                                       model=MODEL, version=MODEL_VERSION, route=ROUTE, thinking=THINKING,
                                       budgets=dict(max_tokens_per_request=MAX_TOKENS, max_requests=MAX_REQUESTS, wall_seconds=TIMEOUT))
                            runs.write(json.dumps(row | metrics) + "\n")
                            runs.flush()
                            print(f"{prompt} pair {pair} arm {order}: {metrics['provider_requests']} requests, "
                                  f"{metrics['wall_seconds']}s, {len(metrics['errors'])} run errors", flush=True)
            with concurrent.futures.ThreadPoolExecutor(max_workers=5) as pool:
                list(pool.map(prompt_pairs, args.prompt or PROMPTS))
    rows = [json.loads(line) for line in (output / "blind.jsonl").read_text().splitlines()]
    rows.sort(key=lambda row: row["id"])
    (output / "blind.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))


if __name__ == "__main__":
    main()
