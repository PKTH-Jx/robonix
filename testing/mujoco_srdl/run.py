# SPDX-License-Identifier: MulanPSL-2.0
"""Verify SRDL with live Nav2 and odometry in the Native MuJoCo deployment."""

import argparse
from contextlib import contextmanager
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import time
from uuid import uuid4

import grpc
from google.protobuf.json_format import MessageToDict

HERE = Path(__file__).resolve().parent
NAV = "robonix/service/navigation/navigate"
POSE = "robonix/service/map/get_pose"
DENY = "SRDL requires a map-frame goal"
STOP = "SRDL navigation boundary reached"
AFTER = "SRDL after observed a map-frame pose"


def check(condition, error):
    if not condition:
        raise RuntimeError(error)


def goal(x, y, yaw, frame):
    return {"goal": {
        "header": {"frame_id": frame, "stamp": {"sec": 0, "nanosec": 0}},
        "pose": {
            "position": {"x": x, "y": y, "z": 0.0},
            "orientation": {"x": 0.0, "y": 0.0, "z": math.sin(yaw / 2), "w": math.cos(yaw / 2)},
        },
    }}


@contextmanager
def odometry(container, output):
    path = output / "odometry.jsonl"
    # A unique node lets the container-side timeout bound a detached docker exec.
    name = "srdl_odom_" + uuid4().hex[:10]
    command = 'source /opt/ros/humble/setup.bash && exec timeout 120s python3 -u - --ros-args -r __node:="$1"'
    with (HERE / "odom.py").open() as script, path.open("w") as data, (output / "odometry.log").open("w") as log:
        process = subprocess.Popen(
            ["docker", "exec", "-i", container, "bash", "-lc", command, "_", name],
            stdin=script, stdout=data, stderr=log,
        )
        try:
            deadline = time.monotonic() + 10
            while path.stat().st_size == 0 and time.monotonic() < deadline and process.poll() is None:
                time.sleep(0.1)
            check(path.stat().st_size > 0, f"No /odom data. See {log.name}")
            yield path
        finally:
            if process.poll() is None:
                subprocess.run(
                    ["docker", "exec", container, "pkill", "-TERM", "-f", f"python3 -u - --ros-args -r __node:={name}$"],
                    capture_output=True, timeout=6,
                )
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)


def samples(path, since):
    lines = path.read_text().splitlines(keepends=True)
    return [sample for line in lines if line.endswith("\n")
            if (sample := json.loads(line))["time"] >= since]


def still(sample):
    return sample["speed"] < 0.025 and abs(sample["yaw_rate"]) < 0.06


def wait_still(path, since):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        readings = samples(path, max(since, time.monotonic() - 0.4))
        if len(readings) >= 5 and readings[-1]["time"] - readings[0]["time"] >= 0.25:
            if all(still(value) for value in readings):
                return
        time.sleep(0.2)
    raise RuntimeError("MuJoCo odometry did not settle after the navigation request")


def moved(path, since):
    readings = samples(path, since)
    check(len(readings) >= 2, "Insufficient odometry samples")
    x, y = readings[0]["x"], readings[0]["y"]
    return max(math.hypot(value["x"] - x, value["y"] - y) for value in readings)


def expect_feedback(nodes, rule, phase, effect, report):
    for node in nodes:
        raw = node.get("operator_detail", "")
        if not raw.startswith("{"):
            continue
        feedback = json.loads(raw)
        for entry in feedback.get("reports", []):
            if (entry["rule"], entry["timing"]) != (rule, phase):
                continue
            error = entry["evaluation_error"] or entry["enforcement_error"]
            check(not error, f"{rule}: {error}")
            if report in feedback["messages"] and feedback["effect"] == effect:
                check(bool(entry["violations"]), f"{rule} had no violated requirement")
                return
    raise RuntimeError(f"Missing {phase} report: {report}")


def run_cases(call, odom, target, record, summarize, model_trace):
    def pose():
        _, leaf, failed = call("pose", "mapping", POSE, {})
        check(leaf.state == 2 and not failed and leaf.leaf_result.success, f"Pose result: {leaf}")
        output = json.loads(leaf.leaf_result.output)
        check(output["ok"], f"Mapping pose unavailable: {output}")
        return leaf, output

    def distance(position):
        return math.hypot(target[0] - position["x"], target[1] - position["y"])

    def before():
        start = time.monotonic()
        nodes, leaf, failed = call("before", "nav2", NAV, goal(*target, "odom"))
        expect_feedback(nodes, "navigation_frame", "before", "deny", DENY)
        check(leaf.state == 3 and failed, "Expected a rejected plan")
        check(all(AFTER not in node.get("operator_detail", "") for node in nodes),
              "After report appeared on rejected navigation")
        _, status, _ = call("denied_status", "nav2", NAV + "/status", {"run_id": ""})
        result = status.leaf_result
        unknown = (
            json.loads(result.output)["known"] is False if result.success
            else result.error.endswith("unknown run_id")
        )
        check(unknown, f"Expected no Nav2 run after deny: {result}")
        time.sleep(0.25)
        check(moved(odom, start) < 0.015, "MuJoCo moved during denied navigation")
        return "Nav2 has no run and MuJoCo stayed stationary after the frame guard."

    def after():
        leaf, position = pose()
        expect_feedback([MessageToDict(leaf, preserving_proto_field_name=True)],
                        "pose_postcondition", "after", "allow", AFTER)
        return f"Map pose ({position['x']:.3f}, {position['y']:.3f}) succeeded with a postcondition report."

    def while_case():
        _, initial = pose()
        check(distance(initial) >= 0.48, "Goal starts too close to the 0.35 m boundary")
        start = time.monotonic() - 0.15
        try:
            nodes, leaf, failed = call("while", "nav2", NAV, goal(*target, "map"))
            check(leaf.state == 4 and failed, f"Expected a canceled navigation: {leaf}")
            expect_feedback(nodes, "navigation_boundary", "while", "stop", STOP)
            deadline = time.monotonic() + 12
            while True:
                _, status, _ = call("nav_status", "nav2", NAV + "/status", {"run_id": ""})
                status = json.loads(status.leaf_result.output)
                if status["state"] == "CANCELED":
                    break
                check(status["state"] not in {"SUCCEEDED", "FAILED"} and time.monotonic() < deadline, status)
                time.sleep(0.25)
            wait_still(odom, time.monotonic())
            _, final = pose()
            travel = moved(odom, start)
            check(travel >= 0.07, f"MuJoCo did not move before cancel ({travel:.3f} m)")
            check(distance(final) < distance(initial) - 0.06, "Robot did not approach the goal")
            return (f"MuJoCo moved {travel:.3f} m and settled. Nav2 CANCELED. "
                    f"Distance to goal {distance(initial):.3f} -> {distance(final):.3f} m.")
        finally:
            _, state, _ = call("cleanup_status", "nav2", NAV + "/status", {"run_id": ""})
            if state.leaf_result.success and json.loads(state.leaf_result.output)["state"] in {"PENDING", "RUNNING"}:
                call("cleanup_cancel", "nav2", NAV + "/cancel", {"run_id": ""})

    if not summarize:
        record("before", before)
        record("after", after)
        record("while", while_case)
        return

    record("summarize while", while_case)
    if model_trace is None:
        return

    def optional_result():
        calls = [json.loads(line) for line in model_trace.read_text().splitlines()]
        boundary = [entry for entry in calls if entry["field"] == "boundary"]
        values = [entry["value"] for entry in boundary]
        check(values[:2] == ["outside", None] and values[-1] == "inside",
              f"Expected an outside result, null, and an inside result: {values}")
        check(boundary[0]["distance"] > 0.35 >= boundary[-1]["distance"],
              "The model did not observe the boundary crossing")
        return f"Continued after a null model result and stopped after inside ({len(values)} samples)."

    def rejected_result(name, expected_error):
        _, leaf, failed = call(name, "mapping", "robonix/service/map/get_mode", {})
        check(leaf.state == 3 and failed and not leaf.leaf_result.success, "Expected a denied capability")
        detail = json.loads(leaf.operator_detail)
        check(detail["effect"] == "deny", detail)
        reports = [report for report in detail["reports"]
                   if report["rule"] == "summarize_probe" and report["timing"] == "before"]
        check(len(reports) == 1, detail)
        error = reports[0]["evaluation_error"] or ""
        check(expected_error in error and not reports[0]["violations"], error or detail)
        return f"Before dispatch was denied with the expected error ({expected_error})."

    record("summarize null", optional_result)
    record("summarize invalid enum", lambda: rejected_result("invalid_enum", "not an allowed enum member"))
    record("summarize HTTP 502", lambda: rejected_result("http_502", "HTTP 502"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deployment", type=Path, required=True)
    parser.add_argument("--goal", type=float, nargs=3, required=True, metavar=("X", "Y", "YAW"))
    parser.add_argument("--executor", default="127.0.0.1:50061")
    parser.add_argument("--container", default=os.environ.get("ROBONIX_SIM_CONTAINER", "mujoco_robonix_srdl"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summarize", action="store_true")
    parser.add_argument("--model-trace", type=Path)
    args = parser.parse_args()
    if not all(math.isfinite(value) for value in args.goal):
        parser.error("goal components must be finite")
    args.output.mkdir(parents=True, exist_ok=False)
    sys.path.insert(0, str(args.deployment / "primitives/chassis/rbnx-build/codegen/proto_gen"))
    from pilot_pb2 import CapabilityCall, Plan, RtdlNode
    from robonix_contracts_pb2_grpc import RobonixSystemExecutorExecuteStub

    results = []
    print(f"Results at {args.output}", flush=True)

    def record(name, action):
        try:
            detail = action()
            status = "PASS"
        except (RuntimeError, ValueError, grpc.RpcError) as exc:
            status, detail = "FAIL", str(exc).splitlines()[0]
        results.append((name, status, detail))
        print(f"{status} {name} - {detail}", flush=True)

    with (args.output / "timeline.jsonl").open("w") as log, grpc.insecure_channel(args.executor) as channel:
        executor = RobonixSystemExecutorExecuteStub(channel)

        def call(name, provider, contract, arguments):
            plan = Plan(plan_id=f"sr-{name}-{uuid4().hex}", nodes=[RtdlNode(
                node_kind=2, op_id=name, description=f"Verify SRDL {name}",
                call=CapabilityCall(call_id=name, provider_id=provider,
                                    contract_id=contract, args_json=json.dumps(arguments)),
            )])
            nodes, leaf, complete = [], None, None
            for event in executor.Execute(plan, timeout=32):
                data = MessageToDict(event, preserving_proto_field_name=True)
                print(json.dumps(dict(time=time.monotonic(), operation=name, event=data)), file=log, flush=True)
                if event.HasField("node_state"):
                    nodes.append(data["node_state"])
                    if event.node_state.HasField("leaf_result"):
                        leaf = event.node_state
                if event.HasField("plan_complete"):
                    complete = event.plan_complete
            check(leaf is not None and complete is not None, f"Incomplete RTDL events for {name}")
            return nodes, leaf, complete.any_failed

        with odometry(args.container, args.output) as odom:
            run_cases(call, odom, args.goal, record, args.summarize, args.model_trace)
    (args.output / "summary.md").write_text(
        "# MuJoCo SRDL\n\n" + "\n".join(f"- **{phase} {status}** — {detail}" for phase, status, detail in results) + "\n",
    )
    return int(any(status != "PASS" for _, status, _ in results))


if __name__ == "__main__":
    sys.exit(main())
