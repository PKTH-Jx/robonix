# Sentinel - capability-call policy

Sentinel evaluates SRDL safety rules for capability calls. `before` checks can
reject dispatch. `after` checks run only after successful execution and report
violations or check errors without changing success or rejecting dependent work.

Declare triggers outside rule blocks:

```text
trigger safety before ["robot/pick", "robot/put_down"];
trigger released after "robot/put_down";
trigger safety while "robot/pick" interval 5;
```

Both sides accept a single item or a nonempty bracketed list. Every listed rule
applies to every listed capability; `always` matches all calls. Rules use `require`
for conditions. Before and after checks run once per event in definition order. Violated rules apply
their bound hooks to one `EnforcementBuilder`. `allow` preserves the accumulated
decision, `deny` makes denial irreversible, and report messages append in order.
`deny` is valid only for `before`; `stop` is valid only for `while` and cancels
the plan containing the checked capability. Type checking keeps them at separate timings.
Later `allow` hooks cannot reverse either effect. Unknown rule references and duplicate
definitions prevent loading. Repeated rule/capability associations at the same
timing also prevent loading, including duplicates within lists and overlaps with
`always`. Different timings and distinct capabilities remain valid.
Unmatched rules skip predicate processing;
before calls still pass exclusion admission.

An enforce clause accepts one hook or a nonempty bracketed list, for example
`enforce deny;`, `enforce [deny];`, or
`enforce [deny, report "blocked", call "robot/stop", allow];`. Multiple hooks require brackets.
Empty lists and trailing commas are rejected. Hooks run in the
listed order; later `allow` hooks preserve denial. `report "TEXT"` can appear
alone or multiple times and appends each message to the result's ordered
`messages` list. Rule reports retain evaluation and enforcement details.

`enforce [call "robot/stop", report "Stop requested"];` records a capability call
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
the same. Check entry points accept a `HookRuntime` for corrective calls and
cancellation.
The evaluator releases its pool entry before hooks or recorded calls execute.
The builder retains all updates through call execution and records exclusion
failures or the committed admission ID. `finish()` produces the final
`EnforcementResult` from that accumulated state.

`exclude A B;` forbids new B calls while A is admitted. `exclude mutual A B;`
declares both directions. Calls awaiting admission reject each other on conflict;
an admitted call continues while later conflicting calls are rejected for replanning.
Relations use exact contract IDs across all plans/providers in this Sentinel instance.

While checks acquire fresh inputs throughout execution. `interval k` sets a
positive finite factor, defaulting to 5. Each rule's start-to-start interval is
`k * avg_check_time`, using the arithmetic mean of that invocation's acquisition
and evaluation times, excluding corrective calls. The first check is immediate;
checks for one invocation never overlap or catch up missed intervals. A violated
rule runs its enforcement clause once until all its requirements pass again.
Unavailable inputs, timeouts, evaluation errors, and failed corrective calls stop
the plan without tolerance. Stop is signalled before deferred calls are handled;
those calls inherit plan cancellation.

Construct `SrdlInterpreter` inside Tokio. Its entry points follow the capability
lifecycle:

- `load(source, client)` parses and type-checks SRDL, binds hooks, and starts the
  shared queues.
- `on_arrival(context, runtime)` reserves exclusions, checks before rules, executes
  corrective calls, and commits admission. `ExclusionChecker` updates the same
  `EnforcementBuilder` used by rule enforcement.
- `on_execution(context, runtime)` yields interval check results until stop or
  cancellation. The stream owns the invocation's schedule and recovery history.
- `on_complete(context, runtime, admission_id, succeeded)` releases exclusions for
  every outcome, then checks after rules only for successful execution.

When execution ends, signal cancellation through the runtime and drain the stream
so started hook calls finish their cancellation handling. The embedding runtime
implements `HookRuntime::stop` using its plan-cancellation path. Normal capability
completion ends monitoring without cancelling the plan. Await arrival checks after signalling cancellation so they can
explicitly discard pending reservations; admission cleanup does not use `Drop`.
Always call `on_complete` with the returned admission ID when handling finishes,
including failure or cancellation. Admission state is bounded to 1024 calls;
exhaustion denies new admission.

`InputAdapter` and `RuleEvaluator` are shared execution queues for all trigger timings.
`RuleEvaluator` owns check scheduling, acquisition completion, freshness retries,
and FIFO evaluation; the interpreter retains its submission channel and capacity.
Each active invocation owns its rules' acquisition state, freshness, retries, and
results, so completing or cancelling it releases that state together.
A stateless `evaluator::evaluate` function calculates predicates against prepared inputs.
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
