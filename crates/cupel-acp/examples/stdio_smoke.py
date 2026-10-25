"""Smoke-test an installed cupel-acp with stdlib-only, local fake inference."""

import asyncio
import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(body)
        chunks = [
            {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "ACP release smoke"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        payload = "".join(f"data: {json.dumps(chunk)}\n\n" for chunk in chunks) + "data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload.encode())))
        self.end_headers()
        self.wfile.write(payload.encode())


async def exchange(binary, home, project):
    # Do not inherit real credentials, project configuration, or proxy settings.
    process = await asyncio.create_subprocess_exec(
        str(binary), "--model", "fixture",
        cwd=project,
        env={"PATH": os.defpath, "HOME": str(home), "CUPEL_HOME": str(home),
             "OLLAMA_HOST": "http://127.0.0.1:9"},
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )

    async def request(identifier, method, params):
        message = {"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}
        process.stdin.write((json.dumps(message) + "\n").encode())
        await process.stdin.drain()
        notifications = []
        while True:
            line = await process.stdout.readline()
            if not line:
                raise RuntimeError(f"unexpected ACP EOF: {(await process.stderr.read()).decode()}")
            message = json.loads(line)
            if message.get("id") == identifier:
                if "error" in message:
                    raise RuntimeError(f"{method}: {message['error']}")
                return message["result"], notifications
            notifications.append(message)

    try:
        initialized, _ = await request(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        assert initialized["protocolVersion"] == 1, initialized
        session, _ = await request(2, "session/new", {"cwd": str(project), "mcpServers": []})
        assert session["sessionId"], session
        result, notifications = await request(3, "session/prompt", {
            "sessionId": session["sessionId"], "prompt": [{"type": "text", "text": "Release smoke"}],
        })
        assert result["stopReason"] == "end_turn", result
        text = "".join(
            update.get("content", {}).get("text", "")
            for message in notifications
            if message.get("method") == "session/update"
            and (update := message["params"]["update"]).get("sessionUpdate") == "agent_message_chunk"
        )
        assert text == "ACP release smoke", notifications
        process.stdin.close()
        status = await asyncio.wait_for(process.wait(), timeout=10)
        assert status == 0, (await process.stderr.read()).decode()
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()


def main():
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="cupel-acp-smoke-") as root:
        root = Path(root).resolve()
        home, project = root / "home", root / "project"
        home.mkdir()
        project.mkdir()
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.requests = []
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        (home / "models.json").write_text(json.dumps([{
            "id": "fixture", "name": "Fixture", "api": "openai-completions", "provider": "fixture",
            "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "reasoning": False, "input": ["text"],
            "cost": {"input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0},
            "contextWindow": 32768, "maxTokens": 1024, "compat": {"requiresApiKey": False},
        }]))
        try:
            async def run():
                await asyncio.wait_for(exchange(binary, home, project), timeout=30)

            asyncio.run(run())
            assert len(server.requests) == 1, server.requests
            print("PASS: packaged cupel-acp initialized, created a session, and completed local inference")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
