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

## Watch the demo in MuJoCo Viewer

Log in to the robonix-vm desktop. Use a terminal in the VM or an SSH
terminal connected to it.

    bash testing/mujoco_srdl/viewer.sh
    bash testing/mujoco_srdl/run.sh

The first command opens the Viewer on the VM desktop. The second starts
Robonix, runs the three cases and leaves the Viewer open. Watch the base
during the while case. It approaches the goal and stops after entering the
0.35 m boundary. The before case leaves the robot still and the after case
reads the pose. The terminal and timeline show their rule reports.

To repeat the same route, restore the initial scene before running the
test again. The reset restarts the MuJoCo container and discards the
current simulation state.

    bash testing/mujoco_srdl/viewer.sh --reset
    bash testing/mujoco_srdl/run.sh

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

## Summarize in MuJoCo

Use the initial robot pose and run the following command.

    bash testing/mujoco_srdl/run.sh --summarize

The test uses the live mapping capability as the observation source and
the local model substitute as the chat completions endpoint. Sentinel
sends the pose, goal and allowed yield values to the endpoint. The
substitute computes outside or inside from the returned map coordinates.
It returns null once while the robot is still outside. The rule permits
that single uncertain observation and continues monitoring. An inside
classification must cancel Nav2 and the simulated base must settle.

Two additional read-only plans use the same observation. The model first
returns a value outside the allowed enum and then returns HTTP 502.
Each plan must be denied before dispatch. The test checks the distinct
evaluation error for each rejection.

model.jsonl records the substitute's HTTP statuses, yield values and poses.
It uses the same clock as cases/timeline.jsonl and cases/odometry.jsonl.

To run the navigation case with a configured model, export
SENTINEL_SUMMARIZE_BASE_URL, SENTINEL_SUMMARIZE_MODEL and
SENTINEL_SUMMARIZE_API_KEY in the VM shell before running this command.

    bash testing/mujoco_srdl/run.sh --summarize-live

The configured endpoint receives live map poses and the goal. The injected
null and error cases are part of the local substitute run.

## Run one group or compare all three

Run the group you want from the Robonix checkout.

    bash testing/mujoco_srdl/suite.sh direct
    bash testing/mujoco_srdl/suite.sh stub
    bash testing/mujoco_srdl/suite.sh live

The direct group checks before, while and after using deterministic SRDL
rules. The stub group checks the Summarize contract with controlled model
replies, including null, invalid enums and HTTP 502. The live group uses
the configured model and checks the resulting cancellation against
MuJoCo odometry. Export the three model settings described above before
running live.

To run the three groups in sequence, run the following command.

    bash testing/mujoco_srdl/suite.sh all

Each group starts from the same scene and uses the same navigation goal
and 0.35 m stop boundary. Add --viewer to reset the scene with the Viewer
before each selected group. The last scene stays open. For a headless
run, close any existing simulation first.

Use all --local to run direct and stub without configuring a model. A
selected live group without model settings is marked skipped and the
command exits nonzero. Every invocation prints the path to a summary.md
with links to the selected results.
