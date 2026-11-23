# BEHAVIOR-1K SRDL tests

Run these cases against the official house_single_floor / picking_up_toys
template. They submit fixed RTDL plans to Executor and use the BEHAVIOR body
and scene providers. The test starts its own Robonix processes on loopback.
It uses the already running simulator and moves the simulated R1Pro in the
navigation case. Use an idle simulation instance. Pilot and model credentials
are unnecessary.

## Run

Use the existing BEHAVIOR deployment with built body and scene packages.
Source its environment and run the following from the directory containing
the Robonix checkout and the deployment.

    source environment-behavior601.sh
    "$BEHAVIOR_PYTHON" robonix/testing/behavior_srdl/run.py --deployment "$PWD/deployment"

The default simulator address is 127.0.0.1:50191. Four consecutive test
ports start at 52451. Use --simulator or --port to change them. The scenario
starts in childs_room_0, denies grasping toy_box_224, and navigates toward
corridor_0. Use --target and --room for a different initial scene. The
destination must let the robot travel beyond the allowed pose box. The
--distance option sets its half-width in metres and defaults to 0.08.

The fixed BEHAVIOR-1K revision fc75a048cb002291dbb8305f33dc56b5267e188b
requires the included [navigation patch](omnigibson-navigation.patch). Apply
it from the OmniGibson source directory before starting the simulator.

    git apply /path/to/robonix/testing/behavior_srdl/omnigibson-navigation.patch

## Expected results

| Phase | Evidence |
| --- | --- |
| Before | The grasp leaf fails with the deny report. The simulator receives no Start for that plan. The denied call produces no after report. |
| While | Navigate is accepted. Scene replies show an in-bounds pose followed by a boundary crossing. Sentinel reports stop, Executor cancels the plan, and the matching simulator operation reaches stopped. |
| After | A Scene read succeeds and returns a known pose. Its successful terminal event reports a deliberately failed postcondition. |

A stop caused by an unavailable observation or an input error fails the
while case. The test requires the specific report, movement samples and the
simulator's cancellation response.

## Results without a UI

The command prints PASS or FAIL for each case and exits nonzero for any
failure. Each run directory under report/ contains summary.md, the generated
rules, and the component logs. Read timeline.jsonl for timestamped RTDL
events, position samples and simulator operation IDs. The timeline records
the test's traffic through a local forwarding gRPC server to the live
simulator. It also records any cleanup stop separately.

## Replay the SRDL cases

Add --replay to the same command to run against a deterministic simulator
protocol fixture. This starts the same Atlas, Executor and providers. A
sequence of in-bounds poses followed by a boundary crossing replaces native
navigation. The replay checks rule evaluation, Executor cancellation and
the Stop RPC. The live run checks robot movement and the native planner.
The terminal and report identify the mode.
