# Schema Discovery & Evolution

## Automatic discovery

When you run `skippr discover`, Skippr connects to the data source, samples records, and infers the complete schema. This includes:

- **Nested structures** — JSON objects become Arrow structs, preserving full nesting depth
- **Arrays** — including arrays of primitives and arrays of structs
- **Type inference** — distinguishes between String, Integer, Long, Double, Boolean, Date, Timestamp (seconds), and TimestampMilli (milliseconds)
- **Namespace separation** — when `TRANSFORM_NAMESPACE_FIELDS` is set, Skippr creates a separate schema per unique value combination (e.g., one table per event type)

The discovered schema is persisted as pipeline metadata in S3.

## Type inference

Skippr uses heuristic type detection:

| Value pattern | Inferred type |
|---|---|
| `true` / `false` | Boolean |
| `123` | Integer |
| `9999999999` (large) | Long |
| `1.23` | Double |
| `"2024-01-15"` | Date |
| `1704067200` (epoch seconds) | Timestamp |
| `1704067200000` (epoch millis) | TimestampMilli |
| Everything else | String |

Numeric values that fall within valid timestamp ranges are automatically promoted to Timestamp or TimestampMilli when the value fits.

## Schema evolution

When Skippr encounters data that doesn't match the current schema, it handles the change automatically:

- **New fields** — added to the schema. The Glue table is updated with the new column.
- **Type widening** — e.g., Integer to Long, Integer to Double. The column type is updated.
- **Type conflicts** — when a field changes to an incompatible type (e.g., Integer to String), affected records are sent to the [deadletter queue](deadletters.md) while valid records continue processing.

Schema changes are logged:

```
Discovered new field: metadata.tags[].value
Updated pipeline metadata in S3: .../metadata.json
```

## Flattening

By default, Skippr preserves nested structure. Set `TRANSFORM_FLATTEN_EVENTS=yes` to flatten all nested fields into dot-separated column names (e.g., `contact.name`, `location.start_geo.lat`).

## Namespaces

When your source contains multiple event types (e.g., `click`, `purchase`, `signup`), set `TRANSFORM_NAMESPACE_FIELDS` to the field(s) that identify the type:

```bash
export TRANSFORM_NAMESPACE_FIELDS=event_type
```

Skippr creates one schema and one Glue table per unique namespace value.

For composite namespaces:

```bash
export TRANSFORM_NAMESPACE_FIELDS=category,sub_category
```
