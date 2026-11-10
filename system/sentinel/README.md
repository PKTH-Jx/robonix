# Sentinel - capability-call policy

Sentinel evaluates SRDL safety rules for capability calls. Executor checks
`require` before dispatch and `ensure` after successful execution, before accepting
success or starting dependent work. Denials and check failures reject the call;
unmatched calls are allowed.

Declare triggers outside rule blocks: `trigger safety @ "robot/move";` applies
rule `safety` to that contract ID. Either side accepts a nonempty bracketed list;
every listed rule applies to every listed capability. `@ always` matches all calls.
Rules run once per call in definition order, including when triggers overlap.
Rules without a trigger association do not run. Unknown rule references and
duplicate rule definitions prevent loading.

The Adapter binds context, acquires inputs, and returns a complete dataset after
checking freshness. The evaluator calculates predicates atomically. A shared pool
retains pending postconditions and schedules ready datasets in FIFO order;
input fetching uses rounds ordered by deadline.

Construct `SrdlInterpreter` inside Tokio. Use `begin` to check preconditions and
retain the returned `CallChecks`; call `ensure` after successful execution, or
drop the handle on failure or cancellation. Each condition group acquires fresh
inputs while retaining the original call context. Rules still require `require`;
`keep` and dynamic rule loading are not implemented.

Runtime logs correlate checks using `check_id` and omit string values and provider
error payloads.

Run the tests from the repository root:

```sh
cargo test -p robonix-sentinel
```
