"""Illegal builder usage. Each line must be a mypy error; `--warn-unused-ignores`
fails the check if any of them stops being one."""

from types import MappingProxyType

import skippr
from skippr import (
    Config,
    DataSinkAthena,
    DataSinkDuckdb,
    DataSinkPostgres,
    DataSourceHttpClient,
    DataSourcePostgres,
    DataSourceStripe,
    DataSourceS3,
    EnvRef,
    Pipeline,
    SchemaSinkGlue,
)

cfg = Config()
src = cfg.data_source("events", DataSourceS3(s3_bucket="b", s3_prefix="p"))
sink = cfg.data_sink("lake", DataSinkDuckdb(warehouse="w", table_namespace="n"))

# A data sink config is not a data source config.
cfg.data_source("x", DataSinkDuckdb(warehouse="w", table_namespace="n"))  # type: ignore[arg-type]

# A pipeline's data_source takes the data source ref, not a sink ref or a string.
Pipeline(data_source=sink)  # type: ignore[arg-type]
Pipeline(data_source="data_sources.events")  # type: ignore[arg-type]

# data_source is required.
Pipeline()  # type: ignore[call-arg]

# Secrets take EnvRef, never plaintext.
DataSinkPostgres(user="u", database="d", password="hunter2")  # type: ignore[arg-type]

# Required plugin fields are required.
DataSourceS3(s3_bucket="b")  # type: ignore[call-arg]

# Wrong field type.
DataSourceS3(s3_bucket="b", s3_prefix="p", batch_size_bytes="1MB")  # type: ignore[arg-type]

# Enum fields take class members, not strings.
DataSourceStripe(
    stripe_account_id="acct",
    start_date="2024-01-01",
    stream_profile="minimal",  # type: ignore[arg-type]
)

# Unpaired sinks link a SchemaSinkRef, not a name.
cfg.data_sink(  # type: ignore[call-overload]
    "athena",
    DataSinkAthena(
        s3_bucket="o", s3_prefix="p", athena_workgroup_name="w", athena_results_s3_bucket="o"
    ),
    schema_sink="glue",
)

# Session takes a PipelineRef, not a name.
skippr.Session("events")  # type: ignore[arg-type]

# Engine storage is a typed class, not a string.
cfg.storage("local")  # type: ignore[arg-type]

# Paired sinks name their schema sink; a SchemaSinkRef is the unpaired form.
glue = cfg.schema_sink(
    "glue",
    SchemaSinkGlue(s3_bucket="o", s3_prefix="p", athena_workgroup_name="w", athena_results_s3_bucket="o"),
)
cfg.data_sink("lake2", DataSinkDuckdb(warehouse="w", table_namespace="n"), schema_sink=glue)  # type: ignore[call-overload]

# List getters return a copy, so in-place mutation is not offered.
DataSourcePostgres(host="h", password=EnvRef("PG")).tables.append("x")  # type: ignore[union-attr]

# List and map setters take a list and a dict, which is what the runtime accepts.
pg = DataSourcePostgres(host="h", password=EnvRef("PG"))
pg.tables = "abc"  # type: ignore[assignment]
http = DataSourceHttpClient(url="https://example.com")
http.headers = MappingProxyType({"a": "b"})  # type: ignore[assignment]
