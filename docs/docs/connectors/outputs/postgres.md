# Postgres Output

Writes record batches to PostgreSQL. Schemas and tables are created automatically when they do not exist.

## How it works

1. Receives batches from the WAL / sink pipeline.
2. Ensures the target schema exists (`CREATE SCHEMA IF NOT EXISTS`).
3. Creates or alters tables to match the incoming schema.
4. Inserts rows using the configured output format.

## Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Postgres
POSTGRES_HOST=localhost
POSTGRES_PORT=5432
POSTGRES_USER=skippr
POSTGRES_PASSWORD=secret
POSTGRES_DATABASE=analytics
POSTGRES_SCHEMA=public
POSTGRES_SSLMODE=prefer
```

Or via YAML pipeline config:

```yaml
data_sinks:
  destination:
    Postgres:
      host: "localhost"
      port: 5432
      user: "skippr"
      password: "${POSTGRES_PASSWORD}"
      database: "analytics"
      schema: "public"
      sslmode: "prefer"
      format: json
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `POSTGRES_HOST` | `localhost` | PostgreSQL host |
| `POSTGRES_PORT` | `5432` | PostgreSQL port |
| `POSTGRES_USER` | | Database user |
| `POSTGRES_PASSWORD` | | Database password |
| `POSTGRES_DATABASE` | | Target database name |
| `POSTGRES_SCHEMA` | `public` | Target schema for tables |
| `POSTGRES_SSLMODE` | | Libpq-style SSL mode (e.g. `disable`, `require`, `prefer`) |
| `host`, `port`, `user`, `password`, `database`, `schema`, `sslmode`, `format` | | YAML equivalents / overrides |

Connection parameters can be split between environment variables and YAML as supported by your pipeline configuration.
