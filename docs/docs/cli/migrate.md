---
title: skipprd migrate
description: Apply SQL schema migrations next to skippr.yml without stopping the pipeline.
---

# skipprd migrate

`skipprd migrate` applies `migrations/*.sql` next to `skippr.yml`. Each file is one atomic `ALTER TABLE` batch. The engine records it in the skippr store so a retry is idempotent.

Python authors those files; the engine never loads `.py`.

```python
from skippr import Migration

Migration(
    "20261020_rename_region",
    up="ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;",
    down="ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;",
).write("migrations")
```

Files use `-- migrate:up` (optional) and `-- migrate:down` (optional, Rename/Promote only).

```bash
skipprd migrate --pipeline bikehire
skipprd sync --pipeline bikehire --once
```

`Session.migrate()` is the same path. `skipprd query --sql "ALTER TABLE …"` journals as `query-<checksum>`. DISABLE still freezes ingest; it is not required to alter.

`down()` is only valid for Rename and Promote. Drop and Merge fail closed if a `-- migrate:down` section is present.

See [ALTER operations](/operations/alter).
