# Sentinel

Sentinel evaluates SRDL safety rules around capability calls. Executor uses it to
check conditions, reject calls, report violations, and stop plans during execution.

```text
rule speed_limit {
    input from "robonix/system/soma/get_health"
        select number velocity = snapshot.actuators[component_id = "left_wheel"].velocity.value
            maxage 0.1;
    require velocity <= 10;
    enforce [deny, report "Slow down the robot"];
}

trigger speed_limit before "robonix/primitive/chassis/move";
```

Triggers run `before` dispatch, `after` successful completion, or `while` a call is
executing. `deny` is valid before dispatch; `stop` is valid during execution and
cancels the containing plan. After checks report failures without changing the
completed call's result. Input and evaluation errors stop plans during while checks.

`exclude "A" "B";` prevents B from starting while A is admitted. Add `mutual` to
exclude both directions. See [test.srdl](test.srdl) for another policy example.

## Inputs and strategies

`select` extracts structured fields. `yield ... using ...` runs a JSON-processing
pipeline to produce declared inputs:

```text
context string target_id = arguments.target_id;
input from "robonix/system/scene/get_scene_graph"
    yield string contents = "Contents: liquid, nonliquid, or unknown."
        using Summarize("Interpret object {target_id}.");
```

Strategies must be registered by the embedding runtime. Construct
`Summarize::new(api_base, model, api_key)`, register it as `"Summarize"` in
`Strategies`, and pass the registry to `SrdlInterpreter::load_with_strategies`.
The API base includes its version prefix, such as `/v1`. Each strategy receives
JSON, rendered string arguments, and the requested yield fields, then returns JSON.
The final output is checked against the declared names and types.

`select`, `yield`, `using`, and `enforce` accept one item or a nonempty bracketed
list. Multiple items require brackets. Bindings share the rule's scope and must
have unique names. Strategy arguments interpolate imported context as `{name}`;
use `{{` and `}}` for literal braces.

`from "cap"` returns capability JSON directly. `from ["cap"]` wraps it under `"0"`;
multiple sources use `"0"`, `"1"`, etc., accessed as `0.field`. Each pipeline starts
from the acquired JSON. Checks acquire fresh inputs; `maxage` measures elapsed
time from response receipt, including processing time. Tracking across checks is
not implemented.

## Embedding and tests

Construct `SrdlInterpreter` inside Tokio. Call `on_arrival` before dispatch,
`on_execution` to monitor execution, and `on_complete` for every outcome to release
admission and run applicable after checks. On cancellation, drain monitoring for
cleanup. The embedding runtime implements `HookRuntime` for corrective capability
calls and plan cancellation. See [the interpreter API](src/interpreter/mod.rs).

Run from the repository root:

```sh
cargo test -p robonix-sentinel
```
