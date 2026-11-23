# SPDX-License-Identifier: MulanPSL-2.0
"""Replay the pose transition used to isolate SRDL from the native planner."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from threading import Lock
from uuid import uuid4

import grpc


@contextmanager
def replay_simulator(proto, stubs, target, room, distance):
    lock = Lock()
    op_id = uuid4().hex
    status = proto.OperationStatus(op_id=op_id, skill="navigate", state="unknown")
    x = 0.0
    reads = 0

    class Embodiment(stubs.EmbodimentServicer):
        def Start(self, request, context):
            with lock:
                if request.skill != "navigate":
                    return proto.StartReply(reason="UNKNOWN_SKILL")
                if status.state != "unknown":
                    return proto.StartReply(reason="RESOURCE_BUSY")
                status.state = "running"
                return proto.StartReply(accepted=True, op_id=op_id)

        def Status(self, request, context):
            with lock:
                if request.op_id != op_id:
                    return proto.OperationStatus(op_id=request.op_id, state="unknown")
                return proto.OperationStatus.FromString(status.SerializeToString())

        def Stop(self, request, context):
            with lock:
                if request.op_id != op_id or status.state != "running":
                    return proto.StopReply(state=status.state)
                status.state = "stopped"
                return proto.StopReply(ok=True, state=status.state)

    class Scene(stubs.SceneServicer):
        def GetObjectContext(self, request, context):
            if request.object_id == target:
                return proto.ObjectContextReply(object=proto.SceneObject(id=target))
            return proto.ObjectContextReply()

        def ListRegions(self, request, context):
            return proto.ListRegionsReply(regions=[proto.Region(id=room)])

        def GetRobotContext(self, request, context):
            nonlocal reads, x
            with lock:
                if status.state == "running":
                    reads += 1
                    if reads >= 4:
                        x = 2 * distance
                return proto.RobotContextReply(x=x, region="replay_start")

    with ThreadPoolExecutor(max_workers=4) as workers:
        server = grpc.server(workers)
        stubs.add_EmbodimentServicer_to_server(Embodiment(), server)
        stubs.add_SceneServicer_to_server(Scene(), server)
        port = server.add_insecure_port("127.0.0.1:0")
        server.start()
        try:
            yield f"127.0.0.1:{port}"
        finally:
            server.stop(0).wait()
