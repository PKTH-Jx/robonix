# MuJoCo SRDL tests

Run the tests on the VM with the existing Native MuJoCo Ranger Mini V3 and
Piper deployment. The runner uses the installed bridge image and the built
Robonix packages. It sends fixed RTDL plans to Executor and records ROS 2
odometry from the bridge. Pilot and model configuration are unnecessary.

## Run

From the Robonix checkout, run the following command.

    bash testing/mujoco_srdl/run.sh

The runner uses the sibling mujoco-ranger-piper deployment by default. Set
MUJOCO_DEPLOYMENT to use another copy of that deployment. It starts Native
MuJoCo if needed, then starts Robonix with a temporary manifest that points
to navigation.srdl. It stops the components it started after collecting
results. Start with the default scenesmith_room_030 robot pose and the
Robonix deployment ports available. An existing idle simulation can be
reused. If the robot has moved, restart the scene or supply a reachable goal
with three arguments in the map frame.

    bash testing/mujoco_srdl/run.sh 4.783 1.436 1.0472

The goal must start at least 0.48 m from the robot so the while case has
room to observe motion before entering the 0.35 m stop boundary.

## Cases

| Phase | Test |
| --- | --- |
| Before | Submit an odom-frame navigation goal. Require the deny report, an unknown Nav2 run, stationary odometry, and no after report. |
| After | Read a live map-frame pose. Require a successful leaf and the report from a deliberately violated postcondition. The rule reads an optional enum and checks it with match. |
| While | Send a map-frame navigation goal beyond the stop boundary. Require the boundary report, a canceled RTDL leaf, Nav2 CANCELED, and odometry showing motion followed by a sustained stop. |

Each run prints PASS or FAIL. The report directory contains the component
logs and a cases directory with summary.md, timeline.jsonl, and
odometry.jsonl. The two JSONL files use the VM monotonic clock so RTDL events
can be compared with odometry samples. A failed observation does not count
as a boundary crossing.

## Additional SRDL checks

The existing Sentinel tests cover parsing, type checking, typed selects,
maxage, optional and enum values, and exclusions independently of MuJoCo.

    cargo test --locked -p robonix-sentinel --lib
    cargo test --locked -p robonix-executor --bin robonix-executor srdl_summarize_stops_a_running_async_call

The Executor test exercises Summarize with a local model substitute and
checks that a running asynchronous capability is canceled.
