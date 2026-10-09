---
title: skipprd metadata
description: Show the schema discover saved for a pipeline, or replace one namespace's fields with a reviewed schema.
---

# skipprd metadata

`skipprd metadata` reads and writes the schema Skipprd keeps for each pipeline: the per-namespace field list that `discover` infers and `sync` uses. Use `metadata show` to review what was discovered, for example before a first sync into a warehouse. Use `metadata apply` when you want to set a namespace's fields yourself: correct a type that inference got wrong (an ID stored as `string` that should be `long`), mark a field as non-nullable, or pin a schema that was reviewed in a pull request. Both commands print JSON, so they are easy to script.

## Usage

```bash
skipprd metadata show  --pipeline <name> [--config <path>]
skipprd metadata apply --pipeline <name> --namespace <namespace> (--schema <file|-> | --schema-json '<json>') [--config <path>]
```

There is no Python equivalent; call the CLI from Python with `subprocess` if you need it in a script.

## Options

### metadata show

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | Required | Pipeline to show. |
| `--output <MODE>` | `json` | Accepts `json` or `text`. Output is JSON in both cases. |

### metadata apply

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | Required | Pipeline to update. |
| `--namespace <NAME>` | Required | Namespace to replace. It is created if it does not exist. |
| `--schema <PATH>` | None | JSON file with the fields, or `-` to read from stdin. |
| `--schema-json <JSON>` | None | The same JSON inline. Takes precedence over `--schema`. One of the two is required. |
| `--evolved` | On | Marks the schema as evolved so schema sinks are synced. Always on; the flag takes no value. |
| `--output <MODE>` | `json` | Accepts `json` or `text`. Output is JSON in both cases. |

### Global flags

| Flag | Default | Description |
|---|---|---|
| `--config <PATH>` | `./skippr.yml` | Config file to read. The schema is stored in the pipeline's state storage: local disk when `skipprd_el_storage_mode` is `local`, otherwise your state bucket. |
| `--log [LEVEL]` | Off | Print logs to stderr. |
| `--wal-storage <MODE>` | `disk` | Must not be `clustered`. |

### Schema file format

```json
{
  "fields": [
    { "name": "ride_id", "field_type": "long", "nullable": false },
    { "name": "station", "field_type": "string" },
    { "name": "started_at", "field_type": "timestamp", "nullable": true }
  ]
}
```

- `name` and `field_type` are required. `type` is accepted as an alias for `field_type`.
- `nullable` defaults to `true`.
- A bare array of fields, without the `fields` wrapper, is also accepted. That means the `fields` array from `metadata show` can be passed straight back.
- Only top-level fields are supported.
- `field_type` is lowercase and one of: `string`, `long` (or `bigint`), `int` (or `integer`), `short` (or `smallint`), `byte` (or `tinyint`), `double`, `float`, `decimal` (or `numeric`), `boolean`, `date`, `time`, `timestamp`, `timestamp_milli`, `binary`, `uuid`, `fixed`, `json`, `record` (or `struct`), `map`, `array`, `null`.

`apply` **replaces** the namespace's whole field list. Any field you leave out of the file is removed from the saved schema, so start from `metadata show` and edit, rather than writing only the fields you want to change.

## Examples

### Review the discovered schema

```bash
skipprd metadata show --pipeline rides
```

```json
{
  "ok": true,
  "pipeline": "rides",
  "namespaces": [
    {
      "namespace": "rides",
      "fields": [
        { "field_type": "long", "name": "duration_s", "nullable": true },
        { "field_type": "boolean", "name": "member", "nullable": true },
        { "field_type": "string", "name": "ride_id", "nullable": true },
        { "field_type": "string", "name": "station", "nullable": true }
      ]
    }
  ]
}
```

Namespaces and fields are sorted by name. A pipeline that has not been discovered yet returns `"namespaces": []`.

### Correct a type and apply it

Inference saw `ride_id` as a string. Save the current fields, edit them, and apply:

1. Export the namespace's fields to a file (this uses [jq](https://jqlang.org); any JSON tool works):

   ```bash
   skipprd metadata show --pipeline rides \
     | jq '.namespaces[] | select(.namespace == "rides") | .fields' \
     > rides-schema.json
   ```

2. Edit `rides-schema.json` and change `ride_id` to `"field_type": "long", "nullable": false`.

3. Apply it:

   ```bash
   skipprd metadata apply --pipeline rides --namespace rides --schema rides-schema.json
   ```

   ```json
   {
     "ok": true,
     "namespace": "rides",
     "fields_written": 4,
     "evolved": true
   }
   ```

4. Confirm:

   ```bash
   skipprd metadata show --pipeline rides
   ```

### Apply from stdin

```bash
cat rides-schema.json | skipprd metadata apply --pipeline rides --namespace rides --schema -
```

### Apply inline JSON

```bash
skipprd metadata apply --pipeline rides --namespace rides \
  --schema-json '{"fields":[{"name":"ride_id","field_type":"long","nullable":false},{"name":"station","field_type":"string"}]}'
```

## Troubleshooting

`apply` prints `"ok": false` with an `error` message and exits `1` when it cannot apply the schema.

| `error` | Cause | Fix |
|---|---|---|
| `one of --schema or --schema-json is required` | No schema was given. | Pass `--schema <file>`, `--schema -`, or `--schema-json`. |
| `unknown field type 'varchar' for field 'x'` | `field_type` is not one of the supported values, or is not lowercase. | Use a type from the list above, for example `string`. |
| `field 'x' must have "field_type"` | A field has no `field_type` (or `type`). | Add the type. |
| `schema JSON must contain a "fields" array` | The JSON is an object without `fields`. | Wrap the list as `{"fields": [...]}` or pass a bare array. |
| `invalid schema JSON: ...` | The file or string is not valid JSON. Shell quoting is a common cause with `--schema-json`. | Put the JSON in a file and use `--schema`. |
| `failed to read schema file '...'` | The path is wrong or unreadable. | Check the path, relative to your current directory. |

If `metadata show` returns no namespaces for a pipeline you have discovered, check that you are using the same `skippr.yml` (and so the same workspace and state storage) that `discover` used.

## Next steps

- [skipprd discover](/cli/discover)
- [Schema discovery and evolution](/concepts/schema)
- [skipprd sync](/cli/sync)
