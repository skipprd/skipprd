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

## Authentication

Authentication uses environment variables. Credentials are never stored in the config file.

| Variable | Default | Description |
|---|---|---|
| `POSTGRES_HOST` | `localhost` | PostgreSQL host |
| `POSTGRES_PORT` | `5432` | PostgreSQL port |
| `POSTGRES_USER` | | Database user |
| `POSTGRES_PASSWORD` | | Database password |
| `POSTGRES_DATABASE` | | Database name (overrides config file) |
| `POSTGRES_SCHEMA` | `public` | Target schema (overrides config file) |
| `POSTGRES_SSLMODE` | | SSL mode (e.g. `disable`, `require`, `prefer`) |

### Example

```bash
export POSTGRES_HOST="localhost"
export POSTGRES_USER="myuser"
export POSTGRES_PASSWORD="mypassword"
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `connection refused` | Check `POSTGRES_HOST` and `POSTGRES_PORT` are correct and the server is running |
| `password authentication failed` | Verify `POSTGRES_USER` and `POSTGRES_PASSWORD` |
| `database "..." does not exist` | Create the database first, or check the `database` field in config |
| SSL errors | Set `POSTGRES_SSLMODE=disable` for local development |
