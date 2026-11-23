# SPDX-License-Identifier: MulanPSL-2.0
"""Run the SRDL lifecycle cases against a live BEHAVIOR-1K simulator."""

from __future__ import annotations

import argparse
import json
import math
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
from functools import partial
from pathlib import Path
from uuid import uuid4

import grpc
from google.protobuf.json_format import MessageToDict
from google.protobuf.message_factory import GetMessageClass

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
POSE = "robonix/system/scene/get_robot_context"
GRASP = "robonix/service/body/grasp"
NAVIGATE = "robonix/service/body/navigate"
BEFORE = "SRDL before denied grasp"
WHILE = "SRDL while stopped navigation"
AFTER = "SRDL after reported a completed call"
TERMINAL = {"failed", "stopped", "succeeded"}


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def message_dict(message):
    return MessageToDict(message, preserving_proto_field_name=True)


class Timeline:
    """Record Executor events and the simulator requests from the test deployment."""

    def __init__(self, stream):
        self.stream = stream
        self.started = time.monotonic()
        self.lock = threading.Lock()
        self.entries = []

    def add(self, kind, **data):
        with self.lock:
            entry = dict(elapsed_s=round(time.monotonic() - self.started, 4), kind=kind, **data)
            self.entries.append(entry)
            print(json.dumps(entry), file=self.stream, flush=True)

    def calls(self, method):
        with self.lock:
            return [entry for entry in self.entries if entry["kind"] == method]

    def replies(self, method):
        return [entry for entry in self.calls(method) if "reply" in entry]


def forward(call, method, timeline, request, context):
    try:
        remaining = context.time_remaining()
        reply = call(request, timeout=min(remaining, 35) if remaining is not None else 35)
    except grpc.RpcError as exc:
        timeline.add(method, request=message_dict(request), error=exc.details())
        context.abort(exc.code(), exc.details())
    data = message_dict(reply)
    if method == "Scene.GetRobotContext":
        data = dict(x=reply.x, y=reply.y, region=reply.region)
    timeline.add(method, request=message_dict(request), reply=data)
    return reply


@contextmanager
def trace_simulator(channel, proto, stubs, timeline):
    with ThreadPoolExecutor(max_workers=8) as workers:
        server = grpc.server(workers)
        for name, descriptor in proto.DESCRIPTOR.services_by_name.items():
            stub = getattr(stubs, f"{name}Stub")(channel)
            handlers = {
                method.name: grpc.unary_unary_rpc_method_handler(
                    partial(forward, getattr(stub, method.name), f"{name}.{method.name}", timeline),
                    request_deserializer=GetMessageClass(method.input_type).FromString,
                    response_serializer=lambda message: message.SerializeToString(),
                )
                for method in descriptor.methods
            }
            server.add_generic_rpc_handlers((grpc.method_handlers_generic_handler(descriptor.full_name, handlers),))
        address = f"127.0.0.1:{server.add_insecure_port('127.0.0.1:0')}"
        server.start()
        try:
            yield address
        finally:
            server.stop(0).wait()


def prepare_rules(target, x, y, distance, output):
    bounds = {
        "min_x": round(x - distance, 6),
        "max_x": round(x + distance, 6),
        "min_y": round(y - distance, 6),
        "max_y": round(y + distance, 6),
    }
    source = (HERE / "fixtures.srdl").read_text().replace("@TARGET@", json.dumps(target))
    for name, value in bounds.items():
        source = source.replace(f"@{name.upper()}@", f"{value:.6f}")
    rules = output / "sentinel-rules.srdl"
    rules.write_text(source)
    return rules, bounds


@contextmanager
def boot(deployment, output, rules, simulator, port):
    paths = {name: deployment / name for name in ("body", "scene")}
    for path in paths.values():
        require((path / "package_manifest.yaml").is_file(), f"Provider package missing at {path}")
    robot_yaml = deployment / "soma.yaml"
    require(robot_yaml.is_file(), f"Soma description missing at {robot_yaml}")

    atlas, executor, _, soma = (f"127.0.0.1:{number}" for number in range(port, port + 4))
    for number in range(port, port + 4):
        with socket.socket() as probe:
            probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            probe.bind(("127.0.0.1", number))

    manifest = {
        "manifestVersion": 1,
        "name": "behavior-srdl-tests",
        "system": {
            "atlas": {"listen": atlas},
            "executor": {"listen": executor, "sentinel_rules": str(rules), "require_keystone": False},
            "soma": {"listen": soma, "robot_yaml": str(robot_yaml)},
        },
        "service": [{"name": name, "path": str(path)} for name, path in paths.items()],
    }
    manifest_path = output / "robonix_manifest.yaml"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    home = output / "robonix-data"
    home.mkdir()
    (home / "config.yaml").write_text(json.dumps({
        "package_storage_path": str(home / "packages"),
        "robonix_source_path": str(ROOT),
    }))
    env = dict(
        os.environ, BENCH_EMBODIMENT_ADDR=simulator, ROBONIX_ATLAS=atlas,
        ROBONIX_DATA_DIR=str(home), ROBONIX_HOME=str(home), ROBONIX_SOURCE_PATH=str(ROOT),
    )

    console = output / "boot.log"
    with console.open("w") as stream:
        process = subprocess.Popen(
            ["rbnx", "boot", "-f", str(manifest_path), "--no-update-check", "--verbose",
             "--log-dir", str(output / "components")],
            env=env, stdin=subprocess.DEVNULL, stdout=stream, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                require(process.poll() is None, f"Robonix exited with {process.returncode}. See {console}")
                if "Ctrl-C to tear down" in console.read_text(errors="replace"):
                    break
                time.sleep(0.25)
            else:
                raise TimeoutError(f"Robonix did not become ready. See {console}")
            yield executor
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=10)
                    raise


def execute(executor, pilot, timeline, case, provider, contract, args):
    plan = pilot.Plan(plan_id=f"srdl-{case}-{uuid4().hex}", nodes=[pilot.RtdlNode(
        node_kind=2, op_id=case, description=f"Verify SRDL {case}",
        call=pilot.CapabilityCall(call_id=case, provider_id=provider, contract_id=contract,
                                  args_json=json.dumps(args)),
    )])
    nodes, complete = [], None
    for event in executor.Execute(plan, timeout=120):
        timeline.add("RTDL", case=case, event=message_dict(event))
        if event.HasField("node_state"):
            nodes.append(event.node_state)
        if event.HasField("plan_complete"):
            complete = event.plan_complete
    require(complete is not None, f"{case} did not return PLAN_COMPLETE")
    leaves = [node for node in nodes if node.HasField("leaf_result")]
    require(len(leaves) == 1, f"{case} did not produce exactly one terminal result")
    return nodes, leaves[0], complete.any_failed


def feedback(nodes, timing, rule, message, effect):
    for node in nodes:
        if not node.operator_detail.startswith("{"):
            continue
        detail = json.loads(node.operator_detail)
        for report in detail.get("reports", []):
            if (report["rule"], report["timing"]) != (rule, timing):
                continue
            error = report["evaluation_error"] or report["enforcement_error"]
            require(error is None, f"{timing} rule {rule}: {error}")
            if detail["effect"] == effect and message in detail["messages"]:
                require(report["violations"], f"{rule} reported without a violated requirement")
                return
    raise RuntimeError(f"Missing Sentinel {timing} report for {rule} ({effect})")


def before_case(execute_case, timeline, target):
    nodes, leaf, failed = execute_case("before", "body", GRASP, {"object": target})
    feedback(nodes, "before", "verify_before", BEFORE, "deny")
    require(leaf.state == 3 and failed and not leaf.leaf_result.success, leaf)
    require(not timeline.calls("Embodiment.Start"), "Rejected grasp reached the simulator")
    require(not any(AFTER in node.operator_detail for node in nodes), "After ran on a denied call")
    return "Grasp denied before Start. No after report."


def after_case(execute_case, timeline):
    reads_before = len(timeline.replies("Scene.GetRobotContext"))
    _, leaf, failed = execute_case("after", "scene", POSE, {})
    feedback([leaf], "after", "verify_after", AFTER, "allow")
    require(leaf.state == 2 and not failed and leaf.leaf_result.success, leaf)
    require(json.loads(leaf.leaf_result.output)["pose_known"] is True, leaf.leaf_result.output)
    require(len(timeline.replies("Scene.GetRobotContext")) > reads_before, "Scene call did not reach the simulator")
    return "Scene read succeeded and its terminal event reported the failed postcondition."


def while_case(execute_case, timeline, body, proto, room, bounds):
    reads_before = len(timeline.replies("Scene.GetRobotContext"))
    nodes, leaf, failed = execute_case("while", "body", NAVIGATE, {"object": "", "place": room})
    require(leaf.state == 4 and failed and not leaf.leaf_result.success,
            f"Expected canceled navigation, got state {leaf.state}: {leaf.leaf_result.error}")
    feedback(nodes, "while", "verify_while", WHILE, "stop")
    starts = timeline.replies("Embodiment.Start")
    require(len(starts) == 1 and starts[0]["reply"].get("accepted"), f"Navigate not accepted: {starts}")
    started = starts[0]
    op_id = started["reply"]["op_id"]
    stops = [call for call in timeline.replies("Embodiment.Stop") if call["request"].get("op_id") == op_id]
    require(any(call["reply"].get("ok") for call in stops), "Executor did not stop the accepted operation")

    reads = timeline.replies("Scene.GetRobotContext")[reads_before:]
    inside, outside = [], []
    for entry in reads:
        if not started["elapsed_s"] <= entry["elapsed_s"] <= stops[0]["elapsed_s"]:
            continue
        x, y = entry["reply"]["x"], entry["reply"]["y"]
        in_bounds = bounds["min_x"] <= x <= bounds["max_x"] and bounds["min_y"] <= y <= bounds["max_y"]
        (inside if in_bounds else outside).append(entry)
    require(inside and outside and inside[0]["elapsed_s"] < outside[-1]["elapsed_s"],
            "Expected a safe pose followed by a boundary crossing during the active operation")

    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        status = body.Status(proto.StatusRequest(op_id=op_id), timeout=5)
        timeline.add("verified_status", reply=message_dict(status))
        if status.state in TERMINAL:
            break
        time.sleep(0.25)
    require(status.state == "stopped", f"Simulator operation ended in {status.state}")
    xyz = outside[0]["reply"]
    return (f"Navigate crossed the boundary at ({xyz['x']:.3f}, {xyz['y']:.3f}). "
            f"Executor canceled the plan and simulator operation {op_id} stopped.")


def stop_unfinished_navigation(timeline, body, proto):
    for call in timeline.replies("Embodiment.Start"):
        if call["request"]["skill"] != "navigate" or not call["reply"].get("accepted"):
            continue
        op_id = call["reply"]["op_id"]
        status = body.Status(proto.StatusRequest(op_id=op_id), timeout=5)
        if status.state in {"accepted", "running"}:
            stopped = body.Stop(proto.StopRequest(op_id=op_id), timeout=5)
            timeline.add("test_cleanup_stop", op_id=op_id, reply=message_dict(stopped))
            require(stopped.ok, f"Could not stop unfinished test operation {op_id}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--deployment", type=Path, required=True, help="BEHAVIOR deployment with built body and scene packages")
    backend_options = parser.add_mutually_exclusive_group()
    backend_options.add_argument("--simulator", default="127.0.0.1:50191")
    backend_options.add_argument("--replay", action="store_true", help="Use a deterministic gRPC pose replay")
    parser.add_argument("--target", default="toy_box_224", help="Object whose grasp is denied")
    parser.add_argument("--room", default="corridor_0", help="Navigation destination for the while case")
    parser.add_argument("--distance", type=float, default=0.08, help="Half-width of the allowed pose box in metres")
    parser.add_argument("--port", type=int, default=52451, help="First of four free consecutive TCP ports")
    parser.add_argument("--output-dir", type=Path, default=HERE / "report")
    args = parser.parse_args()
    if not math.isfinite(args.distance) or args.distance <= 0 or not 1024 <= args.port <= 65532:
        parser.error("distance must be positive and port must start a valid four-port range")
    deployment = args.deployment.resolve()
    codegen = deployment / "body/rbnx-build/codegen/proto_gen"
    require((codegen / "robonix_contracts_pb2_grpc.py").is_file(), f"Executor stubs missing at {codegen}")
    sys.path.insert(0, str(codegen))
    import pilot_pb2 as pilot
    from robonix_contracts_pb2_grpc import RobonixSystemExecutorExecuteStub
    from embodiedosbench.proto import embodiment_pb2 as proto, embodiment_pb2_grpc as stubs

    args.output_dir.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="run-", dir=args.output_dir.resolve()))
    mode = "replay" if args.replay else "live"
    print(f"{mode.title()} run. Artifacts at {output}", flush=True)
    rows = []

    def record(case, check):
        try:
            detail = check()
            result = "PASS"
        except (RuntimeError, ValueError, grpc.RpcError) as exc:
            result, detail = "FAIL", str(exc)
        print(f"{result} {case} - {detail}", flush=True)
        rows.append((case, result, detail))
        timeline.add("case_result", case=case, result=result, detail=detail)

    try:
        with ExitStack() as resources:
            stream = resources.enter_context((output / "timeline.jsonl").open("w"))
            address = args.simulator
            if args.replay:
                from replay import replay_simulator

                address = resources.enter_context(replay_simulator(
                    proto, stubs, args.target, args.room, args.distance,
                ))
            backend = resources.enter_context(grpc.insecure_channel(address))
            timeline = Timeline(stream)
            scene, body = stubs.SceneStub(backend), stubs.EmbodimentStub(backend)
            target = scene.GetObjectContext(
                proto.ObjectContextRequest(object_id=args.target, limit=1), timeout=10,
            )
            require(target.object.id == args.target, f"Target {args.target!r} missing")
            rooms = scene.ListRegions(proto.ListRegionsRequest(), timeout=10)
            require(any(room.id == args.room for room in rooms.regions), f"Unknown room {args.room!r}")
            pose = scene.GetRobotContext(proto.RobotContextRequest(limit=1), timeout=10)
            require(pose.region != args.room, f"Robot is already in {args.room!r}")
            rules, bounds = prepare_rules(args.target, pose.x, pose.y, args.distance, output)
            timeline.add("setup", mode=mode, target=args.target, room=args.room,
                         robot=dict(x=pose.x, y=pose.y), bounds=bounds)
            with trace_simulator(backend, proto, stubs, timeline) as proxy:
                with boot(deployment, output, rules, proxy, args.port) as address, grpc.insecure_channel(address) as channel:
                    execute_case = partial(execute, RobonixSystemExecutorExecuteStub(channel), pilot, timeline)
                    record("before", partial(before_case, execute_case, timeline, args.target))
                    record("after", partial(after_case, execute_case, timeline))
                    try:
                        record("while", partial(
                            while_case, execute_case, timeline, body, proto, args.room, bounds,
                        ))
                    finally:
                        stop_unfinished_navigation(timeline, body, proto)
    except (OSError, RuntimeError, subprocess.SubprocessError, grpc.RpcError) as exc:
        rows.append(("setup or teardown", "FAIL", str(exc)))
        print(f"FAIL setup or teardown - {exc}", file=sys.stderr)
    finally:
        with (output / "summary.md").open("w") as report:
            print(f"# BEHAVIOR-1K SRDL ({mode})\n", file=report)
            for case, result, detail in rows:
                print(f"- **{case}: {result}**", file=report)
                for line in detail.splitlines():
                    print(f"      {line}", file=report)
                print(file=report)
    return int(len(rows) != 3 or any(result != "PASS" for _, result, _ in rows))


if __name__ == "__main__":
    sys.exit(main())
