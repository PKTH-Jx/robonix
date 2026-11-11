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
for conditions and run once per event in definition order. After checks ignore
the enforcement behavior and retain violation messages. Unknown rule references
and duplicate definitions prevent loading. Unmatched rules skip predicate processing;
before calls still pass exclusion admission.

`exclude A B;` forbids new B calls while A is admitted. `exclude mutual A B;`
declares both directions. Calls awaiting admission reject each other on conflict;
an admitted call continues while later conflicting calls are rejected for replanning.
Relations use exact contract IDs across all plans/providers in this Sentinel instance.

Construct `SrdlInterpreter` inside Tokio. Call `check` with `TriggerTiming::Before`
before dispatch and with `TriggerTiming::After` only after successful execution.
Both use the same acquisition, evaluation, timeout and cancellation path; dropping
a check future cancels its work. No evaluator state is retained between checks.
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
