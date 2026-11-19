# Recorded observations

These snapshots come from the MuJoCo Ranger/Piper deployment in
`scenesmith_room_030` on 2026-11-18. `scene-before-explore.json` was captured
before exploration. The other files came from independent calls after the run.

Scene files retain MCP's `result` wrapper. Soma and Vitals retain the gRPC
`snapshot` wrapper, including default scalar values and null absent messages.
The [model cases](../test.rs) pair each observation with a question, yield fields,
and expected values. The reference answers treat Vitals' `-1` measurements as
unavailable and Soma's gripper state as a heuristic.
