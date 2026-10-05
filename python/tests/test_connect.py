import pytest
import skippr
from skippr import DataSource, StorageMode


def test_session_requires_pipeline():
    with pytest.raises(TypeError):
        skippr.Session()


def test_session_pipeline_only_uses_discovered_yml(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    skippr.workspace("bikehire")
    s = skippr.Session(pipeline="bikehire")
    s.connect().data_source(DataSource.S3, skippr.DataSourceS3(s3_bucket="b", s3_prefix="p")).name("sample")
    raw = (tmp_path / "skippr.yml").read_text()
    assert "data_sources.sample" in raw


def test_workspace_and_connect_persist(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("bikehire", config=str(path)).storage_mode(StorageMode.LOCAL)
    s = skippr.Session(pipeline="bikehire", config_file=str(path))
    s.connect().data_source(DataSource.S3, skippr.DataSourceS3(s3_bucket="b", s3_prefix="p")).name("sample")
    raw = path.read_text()
    assert "workspace: bikehire" in raw
    assert "skipprd_el_storage_mode: local" in raw
    assert "s3_bucket: b" in raw
    assert "data_sources.sample" in raw


def test_connect_keeps_sibling_pipeline(tmp_path):
    path = tmp_path / "skippr.yml"
    path.write_text(
        """
skippr:
  workspace: demo
pipelines:
  a:
    data_source: data_sources.src_a
  b:
    data_source: data_sources.src_b
data_sources:
  src_a:
    S3:
      s3_bucket: keep-me
      s3_prefix: old
      version: "1"
  src_b:
    S3:
      s3_bucket: other
      s3_prefix: p
"""
    )
    s = skippr.Session(pipeline="a", config_file=str(path))
    s.connect().data_source(
        DataSource.S3, skippr.DataSourceS3(s3_bucket="keep-me", s3_prefix="new")
    ).name("src_a")
    raw = path.read_text()
    assert "src_b" in raw
    assert "version:" in raw
    assert "s3_prefix: new" in raw or 's3_prefix: "new"' in raw


def test_plaintext_secret_is_rejected(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        s.connect().data_sink(
            skippr.DataSink.Postgres,
            skippr.DataSinkPostgres(host="localhost", password="hunter2"),
        ).name("db")


def test_postgres_password_persists_env_ref(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("bikehire", config=str(path))
    s = skippr.Session(pipeline="bikehire", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.Postgres,
        skippr.DataSinkPostgres(
            host="localhost",
            user="skippr",
            password="${POSTGRES_PASSWORD}",
            database="analytics",
        ),
    ).name("warehouse")
    raw = path.read_text()
    assert "${POSTGRES_PASSWORD}" in raw
    assert "hunter2" not in raw
    assert "password:" in raw


def test_postgres_tables_persist_as_yaml_list(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("bikehire", config=str(path))
    s = skippr.Session(pipeline="bikehire", config_file=str(path))
    s.connect().data_source(
        DataSource.Postgres,
        skippr.DataSourcePostgres(
            host="localhost",
            user="skippr",
            password="${POSTGRES_PASSWORD}",
            database="analytics",
            tables=["orders", "items"],
        ),
    ).name("warehouse")
    raw = path.read_text()
    assert "tables:" in raw
    assert "orders" in raw
    assert "items" in raw


def test_auth_token_path_follows_plugin(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_source(
        DataSource.Otlp, skippr.Otlp(auth_token="${OTLP_TOKEN}")
    ).name("otel")
    otlp = path.read_text()
    assert "auth_token:" in otlp
    assert "${OTLP_TOKEN}" in otlp
    assert "\n    auth:" not in otlp
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        s.connect().data_source(
            DataSource.Otlp, skippr.Otlp(auth_token="hunter2")
        ).name("otel")
    s.connect().data_source(
        DataSource.HttpClient,
        skippr.HttpClient(
            url="https://ex",
            auth=skippr.HttpClientDataSourceHttpAuthConfig(token="${HTTP_TOKEN}"),
        ),
    ).name("http")
    raw = path.read_text()
    http = raw.split("HttpClient:", 1)[1]
    assert "token:" in http
    assert "${HTTP_TOKEN}" in http


def test_numeric_connect_fields_persist_as_yaml_numbers(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_source(
        DataSource.File,
        skippr.DataSourceFile(
            path="/tmp/in",
            format="json",
            batch_size_bytes=1048576,
            batch_size_seconds=5,
        ),
    ).name("local")
    raw = path.read_text()
    assert "batch_size_bytes: 1048576" in raw
    assert "batch_size_seconds: 5" in raw
    assert "batch_size_bytes: '1048576'" not in raw
    assert 'batch_size_bytes: "1048576"' not in raw


def test_google_serp_targets_persist_as_yaml_list(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_source(
        DataSource.GoogleSerpRanks,
        skippr.GoogleSerpRanks(
            keywords=["skippr"],
            targets=[skippr.GoogleSerpRanksTargetEntry(site="example.com", aliases=["ex"])],
        ),
    ).name("serp")
    raw = path.read_text()
    assert "site: example.com" in raw or 'site: "example.com"' in raw
    assert "aliases:" in raw
    assert "ex" in raw


def test_skipprlake_object_store_nested_persist_and_secret(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.SkipprLake,
        skippr.DataSinkSkipprLake(
            warehouse="s3://wh/",
            catalog_table="cat",
            object_store=skippr.DataSinkSkipprLakeWarehouseObjectStore(
                type="r2",
                endpoint="${OBJECTS_S3_ENDPOINT}",
                access_key_id="${OBJECTS_ACCESS_KEY_ID}",
                secret_access_key="${OBJECTS_SECRET_ACCESS_KEY}",
            ),
        ),
    ).name("lake")
    raw = path.read_text()
    assert "type: r2" in raw
    assert "${OBJECTS_SECRET_ACCESS_KEY}" in raw
    with pytest.raises(ValueError, match=r"\$\{ENV\}"):
        s.connect().data_sink(
            skippr.DataSink.SkipprLake,
            skippr.DataSinkSkipprLake(
                warehouse="s3://wh/",
                catalog_table="cat",
                object_store=skippr.DataSinkSkipprLakeWarehouseObjectStore(
                    type="r2",
                    endpoint="https://x",
                    access_key_id="k",
                    secret_access_key="plaintext",
                ),
            ),
        ).name("lake")


def test_athena_iceberg_persist_required_fields(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.AthenaIceberg,
        skippr.DataSinkAthenaIceberg(
            warehouse="s3://wh/",
            glue_database_name="analytics",
            athena_workgroup_name="primary",
            athena_results_s3_bucket="results",
        ),
    ).name("warehouse")
    raw = path.read_text()
    assert "AthenaIceberg:" in raw
    assert "glue_database_name: analytics" in raw
    assert "athena_workgroup_name: primary" in raw
    assert "athena_results_s3_bucket: results" in raw
    assert "catalog:" not in raw
    assert "query_engine:" not in raw
    assert "table_prefix:" not in raw


def test_athena_iceberg_schema_sink_persist(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.AthenaIceberg,
        skippr.DataSinkAthenaIceberg(
            warehouse="s3://wh/",
            glue_database_name="analytics",
            athena_workgroup_name="primary",
            athena_results_s3_bucket="results",
        ),
    ).name("warehouse")
    s.connect().schema_sink(
        skippr.SchemaSink.AthenaIceberg,
        skippr.SchemaSinkAthenaIceberg(
            warehouse="s3://wh/",
            glue_database_name="analytics",
            athena_workgroup_name="primary",
            athena_results_s3_bucket="results",
        ),
    ).name("warehouse_schema")
    raw = path.read_text()
    assert "AthenaIceberg:" in raw
    assert "glue_database_name: analytics" in raw
    assert "schema_sinks:" in raw


def test_duckdb_persist_required_fields(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.Duckdb,
        skippr.DataSinkDuckdb(
            warehouse="file:///tmp/lake",
            table_namespace="bronze",
        ),
    ).name("lake")
    raw = path.read_text()
    assert "Duckdb:" in raw
    assert "warehouse: file:///tmp/lake" in raw
    assert "table_namespace: bronze" in raw
    assert "catalog:" not in raw
    assert "query_engine:" not in raw
    assert "object_store:" not in raw


def test_duckdb_schema_sink_persist(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.Duckdb,
        skippr.DataSinkDuckdb(
            warehouse="file:///tmp/lake",
            table_namespace="bronze",
        ),
    ).name("lake")
    s.connect().schema_sink(
        skippr.SchemaSink.Duckdb,
        skippr.SchemaSinkDuckdb(
            warehouse="file:///tmp/lake",
            table_namespace="bronze",
        ),
    ).name("lake_schema")
    raw = path.read_text()
    assert "Duckdb:" in raw
    assert "table_namespace: bronze" in raw
    assert "schema_sinks:" in raw


def test_schema_sink_attaches_to_data_sink(tmp_path):
    path = tmp_path / "skippr.yml"
    skippr.workspace("demo", config=str(path))
    s = skippr.Session(pipeline="p", config_file=str(path))
    s.connect().data_sink(
        skippr.DataSink.Athena,
        skippr.Athena(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="bikehire",
            athena_results_s3_bucket="out",
        ),
    ).name("lake")
    s.connect().schema_sink(
        skippr.SchemaSink.Glue,
        skippr.Glue(
            s3_bucket="out",
            s3_prefix="p",
            athena_workgroup_name="bikehire",
            athena_results_s3_bucket="out",
            glue_database_name="bikehire",
        ),
    ).name("glue")
    raw = path.read_text()
    assert "schema_sink: schema_sinks.glue" in raw
    assert "data_sink: data_sinks.lake" in raw
    assert "pipelines:\n  p:\n    schema_sink:" not in raw
