# Webots Sentinel demo

The manifests copy `sentinel-rules.v1.srdl` to the robot-local rule file on
first boot. This SRDL example rejects the exact `write_file` capability
when its `path` argument equals `protected.txt`. Other paths and capabilities
are allowed by default; this is a parameter-check example, not a general
filesystem or role-based policy.

```bash
rbnx boot -f examples/webots/robonix_manifest.sentinel-demo.yaml
```

Edit the configured local `.srdl` file and restart Executor to change rules.
The loopback `list_rules` and `replace_rules` capabilities are reserved for
future SRDL reading and dynamic loading. They currently return `Unimplemented`.
