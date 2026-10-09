---
title: Revolut Business
description: Sync Revolut Business account balances, transactions, and transaction legs into your warehouse for treasury and reconciliation.
---

# Revolut Business

Use the Revolut Business source for treasury and reconciliation reporting. Each run lands a daily snapshot of account balances and upserts every transaction and transaction leg in your sync window, so status changes (for example `pending` to `completed`) update the row you already have.

Counterparty names, references, and card details never land. Transactions arrive as amounts, currencies, states, and IDs.

## Before you begin

1. **Set up API access in Revolut Business.** In the Revolut Business web app, go to **Settings → APIs → Business API** and add a certificate. You generate an RSA key pair, upload the public key (X.509 certificate), and set an OAuth redirect URI. Revolut gives you a **client ID**.
2. **Authorise the app once** to get a **refresh token**. Follow Revolut's consent flow for your client ID and exchange the authorisation code for tokens. Keep the refresh token and the private key you generated.
3. **Note your issuer domain.** This is the domain of the redirect URI you registered (for example `example.com`). Skipprd puts it in the signed client assertion it sends to Revolut.
4. **Pick a destination that supports how Revolut lands.** Transaction tables upsert by ID and the balance snapshot replaces each day's partition. Only [Athena Iceberg](/connectors/outputs/athenaiceberg) and [SkipprLake](/connectors/outputs/skipprlake) support both.

To try the integration first, create the certificate in a Revolut Business sandbox account and set `api_base` to `https://sandbox-b2b.revolut.com/api/1.0`.

## Configure

Export the credentials so the config can reference them without storing them:

```bash
export REVOLUT_REFRESH_TOKEN="oa_prod_..."
export REVOLUT_PRIVATE_KEY_PEM="$(cat privatecert.pem)"
```

Add the source and wire it into a pipeline. These examples assume you already have a destination named `lake`.

::: code-group

```python [Python]
from skippr import Config, DataSourceRevolutBusiness, EnvRef, Pipeline

cfg = Config.discover()
revolut = cfg.data_source(
    "revolut",
    DataSourceRevolutBusiness(
        client_id="your-client-id",
        start_date="2024-01-01",
        issuer_domain="example.com",
        refresh_token=EnvRef("REVOLUT_REFRESH_TOKEN"),
        private_key_pem=EnvRef("REVOLUT_PRIVATE_KEY_PEM"),
    ),
)
cfg.pipeline(
    "treasury",
    Pipeline(data_source=revolut, data_sink=cfg.get_data_sink("lake")),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-source revolut-business \
  --pipeline treasury \
  --name revolut \
  --client-id your-client-id \
  --start-date 2024-01-01 \
  --issuer-domain example.com \
  --refresh-token '${REVOLUT_REFRESH_TOKEN}' \
  --private-key-pem '${REVOLUT_PRIVATE_KEY_PEM}'
```

```yaml [YAML]
data_sources:
  revolut:
    RevolutBusiness:
      client_id: your-client-id
      start_date: "2024-01-01"
      issuer_domain: example.com
      refresh_token: ${REVOLUT_REFRESH_TOKEN}
      private_key_pem: ${REVOLUT_PRIVATE_KEY_PEM}

pipelines:
  treasury:
    data_source: data_sources.revolut
    data_sink: data_sinks.lake
```

:::

With the CLI, connect the destination to the same pipeline with `skipprd connect data-sink … --pipeline treasury`.

Check the connection and run one pass:

```bash
skipprd discover --pipeline treasury
skipprd sync --pipeline treasury --once
```

A successful run lands a row in `revolut_sync_run_daily` with `status = ok` and the streams it synced.

Revolut access tokens expire after a short time. Skipprd uses the refresh token and private key to get a fresh one whenever it needs it, so long-running pipelines keep working. You can instead set `access_token` to a token you obtained yourself, which is useful for a one-off run.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `client_id` | string | Required | Client ID from your Revolut Business API certificate. Written to every row as `connection_id`. |
| `start_date` | string (`YYYY-MM-DD`) | Required | Start of the transaction window. |
| `lookback_days` | integer | `90` | Days subtracted from `start_date`. Each run reads transactions created from `start_date` minus this many days up to now. |
| `stream_profile` | string | `console_default` | Which streams to sync: `minimal`, `console_default`, or `full`. See [What gets synced](#what-gets-synced). |
| `streams` | list of strings | Not set | Sync exactly these streams instead of a profile: `accounts`, `transactions`, `transaction_legs`, `health`. Unknown names are ignored. |
| `min_query_interval_ms` | integer | `300` | Minimum milliseconds between transaction page requests. |
| `api_base` | string | `https://b2b.revolut.com/api/1.0` | Revolut Business API base URL. Use `https://sandbox-b2b.revolut.com/api/1.0` for the sandbox. |
| `refresh_token` | secret | Not set | OAuth refresh token, as `${ENV_VAR}`. Required unless you set `access_token`. |
| `private_key_pem` | secret | Not set | PEM-encoded RSA private key matching the certificate you uploaded, as `${ENV_VAR}`. Required with `refresh_token`. |
| `issuer_domain` | string | Not set | Domain of your registered redirect URI. Required with `refresh_token`. You can set the `REVOLUT_ISSUER_DOMAIN` environment variable instead. |
| `access_token` | secret | Not set | A Revolut access token, as `${ENV_VAR}`. When set, the refresh settings are ignored. |
| `privacy.mode` | string | `passthrough` | `passthrough`, `profile`, or `allowlist`. See [Personal data](#personal-data). |
| `privacy.profile` | string | `passthrough` | `passthrough` or `upfoundry_safe`. Setting `upfoundry_safe` switches on profile mode even when `mode` is `passthrough`. |
| `privacy.drop_properties` | list of strings | `[]` | Extra field names to remove (case-insensitive) when the `upfoundry_safe` profile is active. |
| `privacy.keep_properties` | list of strings | `[]` | In `allowlist` mode, the only top-level fields kept on each row. |
| `privacy.hash_properties` | list of strings | `[]` | Accepted but not applied in this version. |
| `privacy.on_violation` | string | `drop` | `drop` or `deadletter`. Accepted but not applied in this version. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `REVOLUT_ISSUER_DOMAIN` | string | Not set | Used as the issuer domain when `issuer_domain` is not set in the config. |

## What gets synced

| Stream | Tables | `minimal` | `console_default` | `full` |
|---|---|:-:|:-:|:-:|
| `accounts` | `revolut_account_snapshot`, `revolut_account_source_raw` | ✓ | ✓ | ✓ |
| `transactions` | `revolut_transaction_fact`, `revolut_transaction_source_raw` | ✓ | ✓ | ✓ |
| `transaction_legs` | `revolut_transaction_leg_fact`, `revolut_transaction_leg_source_raw` | | ✓ | ✓ |
| `health` | `revolut_sync_run_daily` | ✓ | ✓ | ✓ |

`console_default` and `full` currently select the same streams. `revolut_sync_run_daily` gets one row per run (`run_date`, `status`, `streams_synced`, `elapsed_ms`) whichever streams you pick.

| Table | Contents | How it lands |
|---|---|---|
| `revolut_account_snapshot` | Account ID, currency, balance, state, created and updated times | One row per account per `run_date`. Each run replaces that day's rows. |
| `revolut_transaction_fact` | Transaction ID, type, state, currency, amount, created and completed times | Upserted by `transaction_id`. |
| `revolut_transaction_leg_fact` | Leg ID, transaction ID, account ID, currency, absolute amount, `direction` (`credit` or `debit`) | Upserted by `leg_id`. |
| `revolut_*_source_raw` | Record ID, stream, fetch time, and a SHA-256 hash of the record after privacy rules | Upserted by record ID and `ingest_run_date`. Use the hash to see when a record changed. The record body itself is not stored. |

**How runs build on each other.** Every run reads all transactions created from `start_date` minus `lookback_days` up to now, then upserts them. A transaction that settles or is reverted after you first synced it is updated in place. Account balances are a fresh snapshot each run; filter to the latest `run_date` for current balances.

**Deletes.** Revolut doesn't delete transactions; a reversed or declined transaction shows up through its `state`.

**Discover.** `skipprd discover` reads only today's transactions and the `minimal` streams to learn column types.

### Personal data

The curated tables never include account names, counterparties, references, descriptions, or card details. The default `passthrough` mode also removes top-level fields named exactly like personal data (`name`, `reference`, `description`, `iban`, `email`, …) before the record hash is computed.

| Setting | Effect |
|---|---|
| `mode: passthrough` (default) | Removes top-level fields named exactly like personal data. |
| `profile: upfoundry_safe` | Also removes any field, at any depth, whose name *contains* a personal-data term such as `merchant`, `counterparty`, `beneficiary`, `card`, or `iban`, plus every field in `drop_properties`. |
| `mode: allowlist` | Keeps only the top-level fields in `keep_properties`. Always include each table's ID column and date column. |

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `sink '…' does not support write policy MergeByKey …` or `… ReplacePartition …` | Your destination can't upsert or replace partitions. | Use Athena Iceberg or SkipprLake. |
| `access_token or refresh_token is required` | Neither credential reached Skipprd. | Export the variable named in `refresh_token` in the shell that runs `skipprd`, and check the name matches. |
| `private_key_pem is required to refresh access tokens` | `refresh_token` is set but the key is missing. | Export the PEM file contents into the variable named in `private_key_pem`. |
| `issuer_domain is required for JWT client assertion …` | No issuer domain is set. | Set `issuer_domain`, or export `REVOLUT_ISSUER_DOMAIN`. |
| `Revolut HTTP 401` | The refresh token was revoked or expired, or the key doesn't match the uploaded certificate. | Re-run Revolut's consent flow for a new refresh token, and check you're using the private key for that certificate. |
| `Revolut HTTP 401` against the sandbox | `api_base` points at production while your credentials are for the sandbox (or the other way round). | Set `api_base` to match the environment that issued the client ID. |
| `invalid start_date` | The date isn't `YYYY-MM-DD`. | Quote it in YAML: `start_date: "2024-01-01"`. |
| Runs get slower over time | Every run re-reads the whole window from `start_date` minus `lookback_days`. | Move `start_date` later once you have the history you need. |

## Next steps

- [How sources land](/concepts/source-landing-semantics)
- [Athena Iceberg destination](/connectors/outputs/athenaiceberg)
- [skipprd sync](/cli/sync)
