---
title: Telemetry
description: What Skipprd posts to skippr.io, when it posts, and how to turn it off.
---

# Telemetry

Skipprd posts an anonymous usage rollup to `https://skippr.io/api/telemetry` about once an hour, and once more when the process exits, so we can see which connectors and versions people actually run. It is on by default.

The payload is a small JSON object: command (`discover`, `sync`, `query`), catalog connector names, formats, flatten on or off, version, OS, an install UUID stored in Skippr Store, and counts (rows, bytes, duration, schema size, error class). It does not include pipeline names, workspace or tenant names, field names, SQL, YAML, cell values, or error strings.

A successful post writes `store_meta/telemetry/last_post_unix`. The next post, scheduled or on exit, waits until an hour has passed. Missing that key means the first run, including a short `--once`, may post on exit.

## Turn it off

Set `SKIPPR_TELEMETRY=0`, or in `skippr.yml`:

```yaml
skippr:
  telemetry: false
```

YAML wins over the environment. `CI=true` always turns it off, so GitHub Actions never hits skippr.io.

There is no cookie. The install UUID is a Skippr Store key, not a browser identifier.
