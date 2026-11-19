# Sentinel

Sentinel checks SRDL safety rules around Executor capability calls. A `before`
rule can deny dispatch, a `while` rule can stop the running plan, and an `after`
rule can report a violation after a successful call. An `exclude` declaration
covers Executor-managed calls. Before rules can also check the robot's observed state.

Set `system.executor.sentinel_rules` to the SRDL file. Executor loads it at
startup. To use `Summarize`, configure its endpoint, model, and dedicated API
key under `system.executor.sentinel_strategies`. See the
[Executor configuration example](../executor/README.md#keystone-and-sentinel).

## Writing a rule

Use `select` to read a field directly from capability JSON. Use `yield` with
`Summarize` when the rule needs to interpret an observation. For a call with a
`target_id` argument, these clauses require the target container to be covered.

```text
context string target_id = arguments.target_id;
input from "robonix/system/scene/get_scene_graph"
    yield option<enum string { "covered", "uncovered" }> closure = "Cover state"
        using Summarize("Describe the opening of container {target_id}.");
require closure? = "covered" | false;
enforce deny;
```

The enum lists the allowed answers. Sentinel asks the model to return `null`
when the evidence is insufficient. If it does, `| false` makes the requirement fail.
Use `match closure { some(state) => state = "covered", none => false }` to write
both cases explicitly. See the [wheel-speed policy](test.srdl) for a complete
rule with a trigger and structured inputs.

Use HTTPS for a remote model endpoint unless the network is trusted. With MCP
stdio, Executor runs the command registered by the provider under its own
account, so only trusted providers should register stdio endpoints.

## Tests

Run `cargo test -p robonix-sentinel` from the repository root. To try a model
case, set `SENTINEL_SUMMARIZE_API_KEY` and `SENTINEL_SUMMARIZE_MODEL`, then run
`cargo run -p robonix-sentinel --example test -- container_explicit`.
Set `SENTINEL_SUMMARIZE_BASE_URL` for a compatible endpoint. The
[fixture notes](examples/fixtures/README.md) describe the recorded cases.
