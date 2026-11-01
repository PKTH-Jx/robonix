# Sentinel - capability-call policy

Sentinel provides the transport-independent rule model and policy evaluation
for capability calls. Executor evaluates the active rules immediately before
dispatching each RTDL `do` node.

SRDL rules match exact capability IDs or `always`, bind trusted call context
and capability inputs, and evaluate requirements before enforcement. Any
deny or check failure rejects the call; no matching rules allows it.

Executor loads SRDL once at startup. The `list_rules` and `replace_rules`
capabilities are retained for SRDL reading and dynamic loading; their
handlers are not yet implemented.

Run the policy tests from the repository root:

```bash
cargo test -p robonix-sentinel
```
