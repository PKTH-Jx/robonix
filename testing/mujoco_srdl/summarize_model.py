# SPDX-License-Identifier: MulanPSL-2.0
"""Local model substitute that classifies live MuJoCo poses and injects errors."""

import json
import math
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MODEL = "mujoco-srdl-test"
GOAL = re.compile(r"Goal x=([^,]+), y=(.+?)\. Use")
lock = threading.Lock()
boundary_reads = 0
probe_reads = 0


def classify(field, hint, pose):
    global boundary_reads, probe_reads

    x, y = pose["x"], pose["y"]
    if pose.get("ok") is not True or pose.get("frame_id") != "map" or not all(map(math.isfinite, (x, y))):
        raise ValueError("Expected a live finite map-frame pose")
    if field["name"] == "boundary" and field["allowed_values"] == ["outside", "inside"]:
        match = GOAL.match(hint)
        if match is None:
            raise ValueError("Expected an interpolated navigation goal")
        goal_x, goal_y = map(float, match.groups())
        distance = math.hypot(x - goal_x, y - goal_y)
        boundary_reads += 1
        value = "inside" if distance <= 0.35 else "outside"
        if boundary_reads == 2 and value == "outside":
            value = None
        return 200, value, {"goal": [goal_x, goal_y], "distance": distance}
    if field["name"] == "frame" and field["allowed_values"] == ["map", "odom"]:
        probe_reads += 1
        return (200, "invalid-frame", {}) if probe_reads == 1 else (502, None, {})
    raise ValueError("Unexpected Summarize yield field")


class Handler(BaseHTTPRequestHandler):
    """Serve the subset of the chat completions protocol used by Summarize."""

    def log_message(self, _format, *args):
        pass

    def respond(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        if self.path != "/v1/chat/completions" or self.headers.get("Authorization") != f"Bearer {MODEL}":
            self.respond(403, {"error": "unexpected route or test authorization"})
            return
        try:
            length = int(self.headers["Content-Length"])
            if not 0 < length <= 65536:
                raise ValueError("Unexpected model request size")
            request = json.loads(self.rfile.read(length))
            if request["model"] != MODEL or request["response_format"] != {"type": "json_object"}:
                raise ValueError("Unexpected model or response format")
            user = json.loads(request["messages"][-1]["content"])
            field, = user["yield_fields"]
            pose = user["input"]["result"]
            with lock:
                status, value, details = classify(field, user["hint"], pose)
                print(json.dumps(dict(time=time.monotonic(), field=field["name"], status=status,
                                      value=value, pose=[pose["x"], pose["y"]], **details)), flush=True)
        except (KeyError, ValueError, TypeError) as exc:
            self.respond(400, {"error": str(exc)})
            return
        if status == 502:
            self.respond(502, {"error": "injected upstream failure"})
            return
        self.respond(200, {"choices": [{
            "finish_reason": "stop",
            "message": {"content": json.dumps({field["name"]: value})},
        }]})


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        Path(sys.argv[1]).write_text(f"http://127.0.0.1:{server.server_address[1]}/v1\n")
        server.serve_forever()
