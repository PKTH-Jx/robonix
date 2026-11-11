# Sentinel - capability-call policy

Sentinel evaluates SRDL safety rules for capability calls. `before` checks can
reject dispatch. `after` checks run only after successful execution and report
violations or check errors without changing success or rejecting dependent work.

Declare triggers outside rule blocks:

```text
trigger safety before ["robot/pick", "robot/put_down"];
trigger released after "robot/put_down";
```

Both sides accept a single item or a nonempty bracketed list. Every listed rule
applies to every listed capability; `always` matches all calls. Rules use `require`
for conditions and run once per event in definition order. Violated rules apply
their bound hooks to one `EnforcementBuilder`. `allow` preserves the accumulated
decision, `deny` makes denial irreversible, and report messages append in order.
`deny` is valid only before dispatch; the type checker rejects every `after`
trigger bound to a rule containing it. Unknown rule references and duplicate
definitions prevent loading. Unmatched rules skip predicate processing;
before calls still pass exclusion admission.

An enforce clause is a nonempty comma-separated list, for example
`enforce deny, report "blocked", call "robot/stop", allow;`. Hooks run in the
listed order; later `allow` hooks preserve denial. `report "TEXT"` can appear
alone or multiple times and appends each message to the result's ordered
`messages` list. Rule reports retain evaluation and enforcement details.

`enforce call "robot/stop", report "Stop requested";` records a capability call
with empty arguments. Hooks update the builder synchronously; the embedding
runtime executes recorded calls in declaration order before the check finishes,
including when another hook has denied admission. Execution failures deny before
calls and remain feedback for after calls. Passing requirements and evaluation
errors do not invoke hooks.

`interpreter::CheckContext` carries invocation data for the whole SRDL check.
The `enforcer/` module defines `Hook`, `EnforcementBuilder`, and `HookRuntime`.
Hooks receive only the builder; the runtime execution loop receives the timing.
Each built-in owns its timing and argument validation; its builder updates are
infallible. To add one, implement
`Hook` and bind its SRDL declaration in `hooks::bind`; the enforcement loop stays
the same. Use `check_with_runtime` to supply runtime operations for `call` hooks.
The evaluator releases its pool entry before hooks or recorded calls execute.
The builder retains all updates through call execution and exclusion admission;
only then does it produce the final `EnforcementResult`.

`exclude A B;` forbids new B calls while A is admitted. `exclude mutual A B;`
declares both directions. Calls awaiting admission reject each other on conflict;
an admitted call continues while later conflicting calls are rejected for replanning.
Relations use exact contract IDs across all plans/providers in this Sentinel instance.

Construct `SrdlInterpreter` inside Tokio. Call `check` with `TriggerTiming::Before`
before dispatch and with `TriggerTiming::After` only after successful execution.
Both use the same acquisition, evaluation, timeout and cancellation path. Dropping
a check cancels input acquisition; callers using `check_with_runtime` must await
started hook calls so the runtime can finish their cancellation handling. No
evaluator state is retained between checks.
When `admission_id` is returned, call `finish(plan_id, call_id, admission_id)` only
when call handling completes, including failure/cancellation. After checks
do not release exclusions. Admission state is bounded to 1024 calls; exhaustion denies
new admission. Dropping a check only releases admission that has not been committed.
Each check acquires fresh inputs and gets a new timeout budget. The Adapter checks
freshness, and the evaluator calculates each rule atomically. Ready datasets use
FIFO ordering; input fetching uses rounds ordered by deadline.

`keep` and dynamic rule loading are not implemented.

Runtime logs correlate checks using `check_id` and omit string values and provider
error payloads.

Run the tests from the repository root:

```sh
cargo test -p robonix-sentinel
```
