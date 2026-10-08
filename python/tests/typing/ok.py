"""Typed builder usage that must pass `mypy --strict`."""

from collections.abc import Sequence

import pyarrow

import skippr
from skippr import (
    Config,
    DataSinkAthena,
    DataSinkDuckdb,
    DataSinkPostgres,
    DataSourceGoogleSerpRanks,
    DataSourceS3,
    DynamoDbStore,
    EnvRef,
    LocalStorage,
    Pipeline,
    PipelineRef,
    SchemaSinkGlue,
    Transform,
)


def build() -> PipelineRef:
    cfg = Config().workspace("demo").storage(LocalStorage()).store(DynamoDbStore("state"))
    src = cfg.data_source("events", DataSourceS3(s3_bucket="b", s3_prefix="p"))
    lake = cfg.data_sink(
        "lake",
        DataSinkDuckdb(warehouse="file:///tmp/lake", table_namespace="bronze"),
        schema_sink="lake",
    )
    glue = cfg.schema_sink(
        "glue",
        SchemaSinkGlue(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
            glue_database_name="db",
        ),
    )
    cfg.data_sink(
        "athena",
        DataSinkAthena(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="wg",
            athena_results_s3_bucket="out",
        ),
        schema_sink=glue,
    )
    cfg.data_sink(
        "db",
        DataSinkPostgres(user="u", database="d", password=EnvRef("PG_PASSWORD")),
    )
    pipeline = Pipeline(data_source=src, data_sink=lake, transform=Transform(flatten_events=True))
    pipeline.sync_frequency_seconds = 60
    return cfg.pipeline("events", pipeline)


def run(ref: PipelineRef) -> pyarrow.Table:
    session = skippr.Session(ref)
    name: str = session.pipeline
    assert name == ref.name
    return session.query("SELECT 1")


def keywords() -> Sequence[str]:
    serp = DataSourceGoogleSerpRanks(keywords=["k"], targets=[])
    serp.keywords = ["a", "b"]
    return serp.keywords
