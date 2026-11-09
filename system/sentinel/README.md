# Sentinel - capability-call policy

Sentinel provides the transport-independent rule model and policy evaluation
for capability calls. Executor evaluates the active rules immediately before
dispatching each RTDL `do` node.

SRDL rules match exact capability IDs or `always`, bind trusted call context
and capability inputs, and evaluate requirements before enforcement. Any
deny or check failure rejects the call; no matching rules allows it.

Runtime checks log matched rules, bound input and context values, requirement
results, and the final decision at Info level. Evaluation failures use Warn.
A `check_id` correlates concurrent checks. Executor writes these `[sentinel]`
records to its log.

String values and provider error payloads are omitted. A background writer keeps
log I/O off the evaluation path; its 1024-record queue drops records when full and
reports the dropped count when the writer resumes. Queued records may be lost
when the process exits.

Executor loads SRDL once at startup. The `list_rules` and `replace_rules`
capabilities are retained for SRDL reading and dynamic loading; their
handlers are not yet implemented.

Run the policy tests from the repository root:

```bash
cargo test -p robonix-sentinel
```
