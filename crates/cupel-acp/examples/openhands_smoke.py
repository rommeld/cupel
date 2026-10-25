"""Actual OpenHands ACPAgent against Cupel and local fake inference."""
import json
import os
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

os.environ["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
from openhands.sdk.agent import ACPAgent
from openhands.sdk.conversation import Conversation

requests = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(body)
        users = sum(m["role"] == "user" for m in body["messages"])
        chunks = [
            {"choices": [{"index": 0, "delta": {"role": "assistant", "content": f"ACP smoke turn {users}"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        ]
        payload = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload.encode())))
        self.end_headers()
        self.wfile.write(payload.encode())


with tempfile.TemporaryDirectory(prefix="cupel-openhands-") as root:
    root = Path(root)
    home, project = root / "home", root / "project"
    home.mkdir()
    project.mkdir()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (home / "models.json").write_text(json.dumps([{
        "id": "fixture", "name": "Fixture", "api": "openai-completions", "provider": "fixture",
        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "reasoning": False, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0},
        "contextWindow": 32768, "maxTokens": 1024, "compat": {"requiresApiKey": False},
    }]))
    agent = ACPAgent(acp_command=["env", f"CUPEL_HOME={home}", "OLLAMA_HOST=http://127.0.0.1:9", str(Path(sys.argv[1]).resolve()), "--model", "fixture"])
    events = []
    conversation = Conversation(agent=agent, workspace=str(project), callbacks=[events.append])
    try:
        conversation.send_message("First message")
        conversation.run()
        conversation.send_message("Second message")
        conversation.run()
        assert len(requests) == 2, len(requests)
        assert sum(m["role"] == "user" for m in requests[1]["messages"]) == 2
        rendered = "\n".join(str(e) for e in events)
        assert "ACP smoke turn 1" in rendered
        assert "ACP smoke turn 2" in rendered
        print("PASS: OpenHands ACPAgent completed two Cupel turns and retained history")
    finally:
        conversation.close()
        agent.close()
        server.shutdown()
